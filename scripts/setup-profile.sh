#!/bin/sh
set -eu
umask 077

usage() {
  printf '%s\n' \
    'Usage: setup-profile HOST CONFIG PASSPHRASE SSH_KEY KNOWN_HOSTS [MAINTENANCE_KEY|-] [BROWSE_USER]' >&2
  exit 2
}

install_private_dir() {
  directory=$1
  [ ! -L "$directory" ] || {
    printf 'Private directory may not be a symlink: %s\n' "$directory" >&2
    exit 1
  }
  install -d -m 0700 -o root -g root "$directory"
  if [ ! -d "$directory" ] || [ -L "$directory" ] || \
    [ "$(stat -c %u "$directory")" -ne 0 ] || \
    [ "$(stat -c %a "$directory")" != 700 ]; then
    printf 'Could not secure private directory: %s\n' "$directory" >&2
    exit 1
  fi
}

if [ "$#" -lt 5 ] || [ "$#" -gt 7 ]; then
  usage
fi
if [ "$(id -u)" -ne 0 ]; then
  printf '%s\n' 'Run this helper as root, directly or explicitly through pkexec.' >&2
  exit 1
fi
for executable in /usr/bin/boxup /usr/lib/boxup/boxup-root /usr/bin/systemd-analyze; do
  [ -x "$executable" ] || {
    printf 'Required executable is unavailable: %s\n' "$executable" >&2
    exit 1
  }
done
command -v setfacl >/dev/null 2>&1 || {
  printf '%s\n' 'setfacl is required to provision read-only desktop browsing.' >&2
  exit 1
}

profile=${1:-}
config_source=${2:-}
passphrase_source=${3:-}
ssh_key_source=${4:-}
known_hosts_source=${5:-}
maintenance_key_source=${6:--}
browse_user=${7:-${PKEXEC_UID:-}}

case "$profile" in
  ''|*[!A-Za-z0-9_-]*|[-_]*|*[-_]) printf '%s\n' 'Invalid profile name.' >&2; exit 1 ;;
esac
case "$browse_user" in
  ''|-*|*[!A-Za-z0-9_.-]*) printf '%s\n' 'A valid browse user or PKEXEC_UID is required.' >&2; exit 1 ;;
esac

for source in "$config_source" "$passphrase_source" "$ssh_key_source" "$known_hosts_source"; do
  if [ ! -f "$source" ] || [ -L "$source" ]; then
    printf 'Required source is not a regular non-symlink file: %s\n' "$source" >&2
    exit 1
  fi
done
config_source=$(realpath -e -- "$config_source")
passphrase_source=$(realpath -e -- "$passphrase_source")
ssh_key_source=$(realpath -e -- "$ssh_key_source")
known_hosts_source=$(realpath -e -- "$known_hosts_source")
passphrase_size=$(wc -c <"$passphrase_source")
if [ "$passphrase_size" -le 0 ] || [ "$passphrase_size" -gt 16384 ]; then
  printf '%s\n' 'Passphrase file must contain 1 to 16384 bytes.' >&2
  exit 1
fi
if [ "$maintenance_key_source" != - ]; then
  if [ ! -f "$maintenance_key_source" ] || [ -L "$maintenance_key_source" ]; then
    printf 'Maintenance key is not a regular non-symlink file: %s\n' "$maintenance_key_source" >&2
    exit 1
  fi
  maintenance_key_source=$(realpath -e -- "$maintenance_key_source")
fi

system_config="/etc/boxup/$profile.toml"
passphrase="/etc/boxup/$profile.passphrase"
ssh_key="/etc/boxup/${profile}_ed25519"
maintenance_key="/etc/boxup/${profile}_maintenance_ed25519"
index_dir="/var/lib/boxup-index/$profile"
index="$index_dir/index.sqlite3"
lock="/var/lib/boxup/$profile/boxup.lock"
dropin="/etc/systemd/system/boxup-backup-server@$profile.timer.d"
schedule_file="$dropin/schedule.conf"

/usr/bin/boxup --config "$config_source" config validate \
  --system-profile "$system_config"

for target in "$system_config" "$passphrase" "$ssh_key" "$index" "$lock"; do
  { [ ! -e "$target" ] && [ ! -L "$target" ]; } || {
    printf 'Target already exists; refusing to overwrite: %s\n' "$target" >&2
    exit 1
  }
done
if [ "$maintenance_key_source" != - ] && { [ -e "$maintenance_key" ] || [ -L "$maintenance_key" ]; }; then
  printf 'Target already exists; refusing to overwrite: %s\n' "$maintenance_key" >&2
  exit 1
fi
if [ -e /etc/boxup/known_hosts ] || [ -L /etc/boxup/known_hosts ]; then
  if [ ! -f /etc/boxup/known_hosts ] || [ -L /etc/boxup/known_hosts ] || \
    [ "$(stat -c %u /etc/boxup/known_hosts)" -ne 0 ] || \
    [ "$(stat -c %a /etc/boxup/known_hosts)" != 644 ]; then
    printf '%s\n' '/etc/boxup/known_hosts must be a root-owned mode-0644 regular file.' >&2
    exit 1
  fi
  cmp -s "$known_hosts_source" /etc/boxup/known_hosts || {
    printf '%s\n' '/etc/boxup/known_hosts differs; refusing to overwrite it.' >&2
    exit 1
  }
fi
if [ -e "$dropin" ] || [ -L "$dropin" ]; then
  if [ ! -d "$dropin" ] || [ -L "$dropin" ] || \
    [ "$(stat -c %u "$dropin")" -ne 0 ] || \
    [ "$(stat -c %a "$dropin")" != 755 ]; then
    printf 'Timer drop-in directory is not a secure root-owned directory: %s\n' "$dropin" >&2
    exit 1
  fi
fi
{ [ ! -e "$schedule_file" ] && [ ! -L "$schedule_file" ]; } || {
  printf 'Timer schedule already exists; refusing to overwrite: %s\n' "$schedule_file" >&2
  exit 1
}

browse_uid=$(id -u "$browse_user")
browse_gid=$(id -g "$browse_user")
passwd_entry=$(getent passwd "$browse_user")
browse_home=$(printf '%s\n' "$passwd_entry" | cut -d: -f6)
case "$browse_home" in
  /*) ;;
  *) printf '%s\n' 'Browse user has no absolute home directory.' >&2; exit 1 ;;
esac
browse_config="$browse_home/.config/boxup/$profile-browse.toml"
{ [ ! -e "$browse_config" ] && [ ! -L "$browse_config" ]; } || {
  printf 'Browse config already exists; refusing to overwrite: %s\n' "$browse_config" >&2
  exit 1
}
if [ ! -d "$browse_home" ] || [ -L "$browse_home" ]; then
  printf '%s\n' 'Browse home must be an existing non-symlink directory.' >&2
  exit 1
fi
for directory in "$browse_home/.config" "$browse_home/.config/boxup"; do
  if [ -e "$directory" ] || [ -L "$directory" ]; then
    if [ ! -d "$directory" ] || [ -L "$directory" ]; then
      printf 'Browse config parent is not a non-symlink directory: %s\n' "$directory" >&2
      exit 1
    fi
  fi
done

for directory in /etc/boxup /var/lib/boxup /var/cache/boxup \
  /var/lib/boxup-restore /var/lib/boxup-docker /var/lib/boxup-index \
  "/var/lib/boxup/$profile" "/var/cache/boxup/$profile" \
  "/var/lib/boxup-restore/$profile" "/var/lib/boxup-docker/$profile" \
  "$index_dir"; do
  install_private_dir "$directory"
done
install -m 0600 -o root -g root "$config_source" "$system_config"
install -m 0600 -o root -g root "$passphrase_source" "$passphrase"
install -m 0600 -o root -g root "$ssh_key_source" "$ssh_key"
if [ "$maintenance_key_source" != - ]; then
  install -m 0600 -o root -g root "$maintenance_key_source" "$maintenance_key"
fi
if [ -e /etc/boxup/known_hosts ]; then
  :
else
  install -m 0644 -o root -g root "$known_hosts_source" /etc/boxup/known_hosts
fi

/usr/lib/boxup/boxup-root --config "$system_config" validate-config
/usr/lib/boxup/boxup-root --config "$system_config" prepare
setfacl -m "u:$browse_uid:--x" /var/lib/boxup-index
setfacl -m "u:$browse_uid:--x" "$index_dir"
setfacl -m "u:$browse_uid:r--" "$index"
install -m 0600 -o root -g root /dev/null "$lock"
setfacl -m "u:$browse_uid:--x" /var/lib/boxup "/var/lib/boxup/$profile"
setfacl -m "u:$browse_uid:r--" "$lock"

install -d -m 0700 -o "$browse_uid" -g "$browse_gid" "$browse_home/.config/boxup"
browse_temporary=$(mktemp)
trap 'rm -f "$browse_temporary"' EXIT HUP INT TERM
/usr/bin/boxup --config "$system_config" config browse-descriptor \
  --system-profile "$system_config" >"$browse_temporary"
install -m 0600 -o "$browse_uid" -g "$browse_gid" "$browse_temporary" "$browse_config"
rm -f "$browse_temporary"
trap - EXIT HUP INT TERM

tab=$(printf '\t')
schedule=$(/usr/lib/boxup/boxup-root --config "$system_config" print-schedule)
timer_unit=
case "$schedule" in
  due)
    timer_unit="boxup-backup-desktop@$profile.timer"
    ;;
  calendar"$tab"*)
    calendar=${schedule#calendar"$tab"}
    [ "$calendar" != "$schedule" ] || {
      printf '%s\n' 'Invalid calendar output from boxup-root.' >&2
      exit 1
    }
    /usr/bin/systemd-analyze calendar "$calendar" >/dev/null
    install -d -m 0755 -o root -g root "$dropin"
    schedule_temporary=$(mktemp)
    trap 'rm -f "$schedule_temporary"' EXIT HUP INT TERM
    {
      printf '%s\n' '[Timer]'
      printf '%s\n' 'OnCalendar='
      printf 'OnCalendar=%s\n' "$calendar"
    } >"$schedule_temporary"
    install -m 0644 -o root -g root "$schedule_temporary" "$schedule_file"
    rm -f "$schedule_temporary"
    trap - EXIT HUP INT TERM
    timer_unit="boxup-backup-server@$profile.timer"
    ;;
  *) printf '%s\n' 'Unsupported schedule output from boxup-root.' >&2; exit 1 ;;
esac

printf 'Created root profile and credentials for %s.\n' "$profile"
printf 'Created secret-free read-only browse descriptor for %s at %s.\n' "$browse_user" "$browse_config"
printf 'Configured schedule uses %s; review it before enabling that unit.\n' "$timer_unit"
printf '%s\n' 'No repository was initialized, no timer was enabled, and systemd was not reloaded.'
