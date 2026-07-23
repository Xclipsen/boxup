#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/boxup-units.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM
root="$work_dir/root"
unit_dir="$root/usr/lib/systemd/system"
install -d "$unit_dir"
install -Dm755 /bin/true "$root/usr/lib/boxup/boxup-root"
for target in basic.target network-online.target shutdown.target sysinit.target timers.target; do
  {
    printf '%s\n' '[Unit]'
    printf 'Description=Temporary %s for source-tree verification\n' "$target"
  } >"$unit_dir/$target"
done

for unit in "$project_dir"/packaging/systemd/*.service; do
  case "$(basename "$unit")" in
    boxup-backup-now@.service) operation=backup ;;
    boxup-backup-due@.service) operation=due ;;
    boxup-maintenance@.service) operation=maintenance ;;
    boxup-check@.service) operation=check ;;
    *) printf 'Unexpected service: %s\n' "$unit" >&2; exit 1 ;;
  esac
  grep -Fqx "ExecStart=/usr/lib/boxup/boxup-root --config /etc/boxup/%i.toml $operation" "$unit"
  grep -Fqx 'NoNewPrivileges=yes' "$unit"
  grep -Fqx 'PrivateTmp=yes' "$unit"
  grep -Fqx 'PrivateDevices=yes' "$unit"
  grep -Fqx 'ProtectSystem=strict' "$unit"
  grep -Fqx 'ProtectHome=read-only' "$unit"
  grep -Fqx 'RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6' "$unit"
  grep -Fqx 'RestrictNamespaces=yes' "$unit"
  grep -Fqx 'UMask=0077' "$unit"
  grep -Eq '^TimeoutStartSec=[1-9][0-9]*(s|min|h|d)$' "$unit"
  grep -Eq '^TimeoutStopSec=[1-9][0-9]*(s|min|h|d)$' "$unit"
  grep -Fqx 'KillSignal=SIGTERM' "$unit"
  grep -Fqx 'KillMode=mixed' "$unit"
  if grep -Eq '^(EnvironmentFile|Environment|RootDirectory|BindPaths)=' "$unit"; then
    printf 'Unsafe or unexpected service directive in %s.\n' "$unit" >&2
    exit 1
  fi
  case "$operation" in
    backup|due)
      expected='ReadWritePaths=/var/lib/boxup/%i /var/cache/boxup/%i /var/lib/boxup-index/%i /var/lib/boxup-docker/%i'
      ;;
    maintenance|check)
      expected='ReadWritePaths=/var/lib/boxup/%i /var/cache/boxup/%i /var/lib/boxup-index/%i'
      ;;
  esac
  grep -Fqx "$expected" "$unit"
  install -m644 "$unit" "$unit_dir/$(basename "$unit")"
done
for unit in "$project_dir"/packaging/systemd/*.timer; do
  install -m644 "$unit" "$unit_dir/$(basename "$unit")"
done

grep -Fqx 'OnCalendar=*-*-* 04:00:00 UTC' \
  "$project_dir/packaging/systemd/boxup-backup-server@.timer"
grep -Fqx 'OnCalendar=Sun *-*-* 06:00:00 UTC' \
  "$project_dir/packaging/systemd/boxup-maintenance@.timer"
grep -Fqx 'OnCalendar=*-*-01 08:00:00 UTC' \
  "$project_dir/packaging/systemd/boxup-check@.timer"
grep -Fqx 'OnUnitActiveSec=4h' \
  "$project_dir/packaging/systemd/boxup-backup-desktop@.timer"
grep -Fqx 'Unit=boxup-backup-due@%i.service' \
  "$project_dir/packaging/systemd/boxup-backup-desktop@.timer"
grep -Fqx 'Unit=boxup-backup-now@%i.service' \
  "$project_dir/packaging/systemd/boxup-backup-server@.timer"

systemd-analyze --root="$root" verify "$unit_dir"/*.service "$unit_dir"/*.timer
