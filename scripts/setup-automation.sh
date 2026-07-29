#!/bin/sh
set -eu

usage() {
  printf '%s\n' 'Usage: setup-automation HOST enable|disable' >&2
  exit 2
}

fail() {
  printf '%s\n' "$1" >&2
  exit 1
}

test_marker=${BOXUP_AUTOMATION_TEST_ONLY:-}
case "$test_marker" in
  '')
    if [ "${BOXUP_AUTOMATION_SYSTEMCTL+x}${BOXUP_AUTOMATION_BOXUP_ROOT+x}${BOXUP_AUTOMATION_CONFIG_DIR+x}${BOXUP_AUTOMATION_STATE_ROOT+x}" ]; then
      fail 'Test path overrides require BOXUP_AUTOMATION_TEST_ONLY=1.'
    fi
    systemctl_path=/usr/bin/systemctl
    boxup_root_path=/usr/lib/boxup/boxup-root
    config_dir=/etc/boxup
    state_root=/var/lib/boxup
    required_owner=0
    [ "$(id -u)" -eq 0 ] || fail 'Run this helper as root, directly or explicitly through pkexec.'
    ;;
  1)
    script_path=$(realpath -e -- "$0") || fail 'Could not resolve the test helper path.'
    [ "$script_path" != /usr/lib/boxup/setup-automation ] || \
      fail 'Test overrides are disabled for the installed helper.'
    if [ -z "${BOXUP_AUTOMATION_SYSTEMCTL:-}" ] || \
       [ -z "${BOXUP_AUTOMATION_BOXUP_ROOT:-}" ] || \
       [ -z "${BOXUP_AUTOMATION_CONFIG_DIR:-}" ] || \
       [ -z "${BOXUP_AUTOMATION_STATE_ROOT:-}" ]; then
      fail 'Test mode requires fixed systemctl, boxup-root, and config paths.'
    fi
    systemctl_path=$(realpath -e -- "$BOXUP_AUTOMATION_SYSTEMCTL") || \
      fail 'Could not resolve the test systemctl path.'
    boxup_root_path=$(realpath -e -- "$BOXUP_AUTOMATION_BOXUP_ROOT") || \
      fail 'Could not resolve the test boxup-root path.'
    config_dir=$(realpath -e -- "$BOXUP_AUTOMATION_CONFIG_DIR") || \
      fail 'Could not resolve the test config directory.'
    state_root=$(realpath -e -- "$BOXUP_AUTOMATION_STATE_ROOT") || \
      fail 'Could not resolve the test state root.'
    if [ "$systemctl_path" = /usr/bin/systemctl ] || \
       [ "$boxup_root_path" = /usr/lib/boxup/boxup-root ] || \
       [ "$config_dir" = /etc/boxup ] || [ "$state_root" = /var/lib/boxup ]; then
      fail 'Test mode may not use installed system paths.'
    fi
    required_owner=$(id -u)
    ;;
  *) fail 'BOXUP_AUTOMATION_TEST_ONLY must be unset or 1.' ;;
esac

[ "$#" -eq 2 ] || usage
profile=$1
action=$2
case "$profile" in
  ''|*[!A-Za-z0-9_-]*|[-_]*|*[-_]) fail 'Invalid profile name.' ;;
esac
case "$action" in
  enable|disable) ;;
  *) usage ;;
esac

[ -x "$systemctl_path" ] || fail 'Required systemctl executable is unavailable.'
[ -x "$boxup_root_path" ] || fail 'Required boxup-root executable is unavailable.'

config="$config_dir/$profile.toml"
validation_marker="$state_root/$profile/requires-live-validation"
success_stamp="$state_root/$profile/last-success.json"
if [ ! -f "$config" ] || [ -L "$config" ]; then
  fail 'The system profile must be a regular non-symlink file.'
fi
canonical_config=$(realpath -e -- "$config") || fail 'Could not canonicalize the system profile.'
[ "$canonical_config" = "$config" ] || fail 'The system profile path is not canonical.'
[ "$(stat -c %u "$config")" -eq "$required_owner" ] || \
  fail 'The system profile must be root-owned.'
profile_mode=$(stat -c %a "$config")
[ $((0$profile_mode & 0022)) -eq 0 ] || \
  fail 'The system profile must not be group or other writable.'
if [ "$action" = enable ]; then
  if [ -L "$validation_marker" ] || [ -e "$validation_marker" ]; then
    fail 'Run a successful live snapshot validation before enabling automation.'
  fi
  if [ ! -f "$success_stamp" ] || [ -L "$success_stamp" ]; then
    fail 'Run one successful manual backup before enabling automation.'
  fi
fi

tab=$(printf '\t')
newline='
'
carriage=$(printf '\r')
schedule=$("$boxup_root_path" --config "$config" print-schedule)
case "$schedule" in
  *"$newline"*|*"$carriage"*) fail 'Unsupported schedule output from boxup-root.' ;;
  due)
    backup_timer="boxup-backup-desktop@$profile.timer"
    other_backup_timer="boxup-backup-server@$profile.timer"
    ;;
  calendar"$tab"*)
    calendar=${schedule#calendar"$tab"}
    case "$calendar" in
      ''|*"$tab"*) fail 'Unsupported schedule output from boxup-root.' ;;
    esac
    backup_timer="boxup-backup-server@$profile.timer"
    other_backup_timer="boxup-backup-desktop@$profile.timer"
    ;;
  *) fail 'Unsupported schedule output from boxup-root.' ;;
esac
desktop_timer="boxup-backup-desktop@$profile.timer"
server_timer="boxup-backup-server@$profile.timer"
index_timer="boxup-index@$profile.timer"

is_enabled() {
  status=0
  "$systemctl_path" is-enabled --quiet "$1" || status=$?
  case "$status" in
    0) printf '%s' true ;;
    1) printf '%s' false ;;
    *) return 2 ;;
  esac
}

is_active() {
  status=0
  "$systemctl_path" is-active --quiet "$1" || status=$?
  case "$status" in
    0) printf '%s' true ;;
    3) printf '%s' false ;;
    *) return 2 ;;
  esac
}

set_unit_state() {
  unit=$1
  enabled=$2
  active=$3
  result=0
  if [ "$enabled" = true ]; then
    "$systemctl_path" enable "$unit" >/dev/null || result=1
  else
    "$systemctl_path" disable "$unit" >/dev/null || result=1
  fi
  if [ "$active" = true ]; then
    "$systemctl_path" start "$unit" >/dev/null || result=1
  else
    "$systemctl_path" stop "$unit" >/dev/null || result=1
  fi
  return "$result"
}

desktop_before=$(is_enabled "$desktop_timer")
server_before=$(is_enabled "$server_timer")
index_before=$(is_enabled "$index_timer")
desktop_active_before=$(is_active "$desktop_timer")
server_active_before=$(is_active "$server_timer")
index_active_before=$(is_active "$index_timer")

rollback() {
  set +e
  rollback_result=0
  set_unit_state "$desktop_timer" "$desktop_before" "$desktop_active_before" || rollback_result=1
  set_unit_state "$server_timer" "$server_before" "$server_active_before" || rollback_result=1
  set_unit_state "$index_timer" "$index_before" "$index_active_before" || rollback_result=1
  [ "$(is_enabled "$desktop_timer")" = "$desktop_before" ] || rollback_result=1
  [ "$(is_enabled "$server_timer")" = "$server_before" ] || rollback_result=1
  [ "$(is_enabled "$index_timer")" = "$index_before" ] || rollback_result=1
  [ "$(is_active "$desktop_timer")" = "$desktop_active_before" ] || rollback_result=1
  [ "$(is_active "$server_timer")" = "$server_active_before" ] || rollback_result=1
  [ "$(is_active "$index_timer")" = "$index_active_before" ] || rollback_result=1
  set -e
  return "$rollback_result"
}

case "$action" in
  enable)
    "$systemctl_path" daemon-reload >/dev/null || fail 'Could not reload systemd.'
    if ! "$systemctl_path" disable --now "$other_backup_timer" >/dev/null ||
       ! "$systemctl_path" enable --now "$backup_timer" >/dev/null ||
       ! "$systemctl_path" enable --now "$index_timer" >/dev/null; then
      rollback || fail 'Could not enable Boxup automation and rollback was incomplete.'
      fail 'Could not enable Boxup automation; previous timer state was restored.'
    fi
    ;;
  disable)
    if ! "$systemctl_path" disable --now "$desktop_timer" >/dev/null ||
       ! "$systemctl_path" disable --now "$server_timer" >/dev/null ||
       ! "$systemctl_path" disable --now "$index_timer" >/dev/null; then
      rollback || fail 'Could not disable Boxup automation and rollback was incomplete.'
      fail 'Could not disable Boxup automation; previous timer state was restored.'
    fi
    ;;
esac

desktop_enabled=$(is_enabled "$desktop_timer")
server_enabled=$(is_enabled "$server_timer")
index_enabled=$(is_enabled "$index_timer")
desktop_active=$(is_active "$desktop_timer")
server_active=$(is_active "$server_timer")
index_active=$(is_active "$index_timer")
printf 'host=%s\taction=%s\tbackup_unit=%s\tdesktop_enabled=%s\tserver_enabled=%s\tindex_enabled=%s\n' \
  "$profile" "$action" "$backup_timer" "$desktop_enabled" "$server_enabled" "$index_enabled"

if [ "$action" = enable ]; then
  if [ "$backup_timer" = "$desktop_timer" ]; then
    expected_desktop=true
    expected_server=false
  else
    expected_desktop=false
    expected_server=true
  fi
else
  expected_desktop=false
  expected_server=false
fi
if [ "$action" = enable ]; then
  expected_index=true
  expected_desktop_active=$expected_desktop
  expected_server_active=$expected_server
  expected_index_active=true
else
  expected_index=false
  expected_desktop_active=false
  expected_server_active=false
  expected_index_active=false
fi
if [ "$desktop_enabled" != "$expected_desktop" ] || \
   [ "$server_enabled" != "$expected_server" ] || \
   [ "$index_enabled" != "$expected_index" ] || \
   [ "$desktop_active" != "$expected_desktop_active" ] || \
   [ "$server_active" != "$expected_server_active" ] || \
   [ "$index_active" != "$expected_index_active" ]; then
    rollback || fail 'Automation verification failed and rollback was incomplete.'
    fail 'Automation verification failed; previous timer state was restored.'
fi
