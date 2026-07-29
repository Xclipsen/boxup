#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
helper="$project_dir/scripts/setup-automation.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/boxup-automation.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM
config_dir="$work_dir/config"
state_dir="$work_dir/state"
active_dir="$work_dir/active"
boxup_state="$work_dir/boxup-state"
mkdir "$config_dir" "$state_dir" "$active_dir" "$boxup_state"
systemctl_log="$work_dir/systemctl.log"
root_log="$work_dir/boxup-root.log"
schedule_file="$work_dir/schedule"
fake_systemctl="$work_dir/systemctl"
fake_boxup_root="$work_dir/boxup-root"

cat >"$fake_systemctl" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$BOXUP_TEST_SYSTEMCTL_LOG"
if [ -n "${BOXUP_TEST_FAIL_ONCE:-}" ] && [ "$*" = "$BOXUP_TEST_FAIL_ONCE" ] &&
   [ ! -e "$BOXUP_TEST_FAIL_MARKER" ]; then
  : >"$BOXUP_TEST_FAIL_MARKER"
  exit 1
fi
case "${1:-}" in
  daemon-reload)
    [ "$#" -eq 1 ]
    ;;
  disable)
    if [ "${2:-}" = --now ]; then
      [ "$#" -eq 3 ]
      rm -f "$BOXUP_TEST_STATE/$3" "$BOXUP_TEST_ACTIVE/$3"
    else
      [ "$#" -eq 2 ]
      rm -f "$BOXUP_TEST_STATE/$2"
    fi
    ;;
  enable)
    if [ "${2:-}" = --now ]; then
      [ "$#" -eq 3 ]
      : >"$BOXUP_TEST_STATE/$3"
      : >"$BOXUP_TEST_ACTIVE/$3"
    else
      [ "$#" -eq 2 ]
      : >"$BOXUP_TEST_STATE/$2"
    fi
    ;;
  is-enabled)
    [ "$#" -eq 3 ] && [ "$2" = --quiet ]
    if [ -f "$BOXUP_TEST_STATE/$3" ]; then exit 0; else exit 1; fi
    ;;
  is-active)
    [ "$#" -eq 3 ] && [ "$2" = --quiet ]
    if [ -f "$BOXUP_TEST_ACTIVE/$3" ]; then exit 0; else exit 3; fi
    ;;
  start)
    [ "$#" -eq 2 ]
    : >"$BOXUP_TEST_ACTIVE/$2"
    ;;
  stop)
    [ "$#" -eq 2 ]
    rm -f "$BOXUP_TEST_ACTIVE/$2"
    ;;
  *) exit 64 ;;
esac
EOF
cat >"$fake_boxup_root" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$BOXUP_TEST_ROOT_LOG"
[ "$#" -eq 3 ]
[ "$1" = --config ]
[ "$2" = "$BOXUP_TEST_CONFIG" ]
[ "$3" = print-schedule ]
IFS= read -r schedule <"$BOXUP_TEST_SCHEDULE"
printf '%s\n' "$schedule"
EOF
chmod 0755 "$fake_systemctl" "$fake_boxup_root"

profile=safe-host
config="$config_dir/$profile.toml"
: >"$config"
chmod 0600 "$config"
export BOXUP_AUTOMATION_TEST_ONLY=1
export BOXUP_AUTOMATION_SYSTEMCTL="$fake_systemctl"
export BOXUP_AUTOMATION_BOXUP_ROOT="$fake_boxup_root"
export BOXUP_AUTOMATION_CONFIG_DIR="$config_dir"
export BOXUP_AUTOMATION_STATE_ROOT="$boxup_state"
export BOXUP_TEST_SYSTEMCTL_LOG="$systemctl_log"
export BOXUP_TEST_ROOT_LOG="$root_log"
export BOXUP_TEST_STATE="$state_dir"
export BOXUP_TEST_ACTIVE="$active_dir"
export BOXUP_TEST_SCHEDULE="$schedule_file"
export BOXUP_TEST_CONFIG="$config"
export BOXUP_TEST_FAIL_MARKER="$work_dir/fail-marker"

run_helper() {
  sh "$helper" "$@"
}

assert_output() {
  [ "$1" = "$2" ] || {
    printf 'Unexpected helper output:\n%s\n' "$1" >&2
    exit 1
  }
}

assert_log() {
  expected="$work_dir/expected.log"
  shift
  : >"$expected"
  for line in "$@"; do
    printf '%s\n' "$line" >>"$expected"
  done
  cmp -s "$expected" "$systemctl_log" || {
    printf '%s\n' 'Unexpected systemctl invocation sequence.' >&2
    exit 1
  }
}

desktop="boxup-backup-desktop@$profile.timer"
server="boxup-backup-server@$profile.timer"
index="boxup-index@$profile.timer"
mkdir -p "$boxup_state/$profile"
: >"$boxup_state/$profile/last-success.json"

printf '%s\n' due >"$schedule_file"
: >"$state_dir/$server"
: >"$active_dir/$server"
: >"$systemctl_log"
: >"$root_log"
output=$(run_helper "$profile" enable)
assert_output "$output" \
  "host=$profile	action=enable	backup_unit=$desktop	desktop_enabled=true	server_enabled=false	index_enabled=true"
assert_log ignored \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index" \
  'daemon-reload' \
  "disable --now $server" \
  "enable --now $desktop" \
  "enable --now $index" \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index"
[ "$(wc -l <"$root_log")" -eq 1 ]

rm -f "$state_dir"/* "$active_dir"/*
: >"$state_dir/$desktop"
: >"$active_dir/$desktop"
printf 'calendar\t*-*-* 04:00:00 UTC\n' >"$schedule_file"
: >"$systemctl_log"
output=$(run_helper "$profile" enable)
assert_output "$output" \
  "host=$profile	action=enable	backup_unit=$server	desktop_enabled=false	server_enabled=true	index_enabled=true"
assert_log ignored \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index" \
  'daemon-reload' \
  "disable --now $desktop" \
  "enable --now $server" \
  "enable --now $index" \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index"

: >"$state_dir/$desktop"
: >"$state_dir/$server"
: >"$state_dir/$index"
: >"$active_dir/$desktop"
: >"$active_dir/$server"
: >"$active_dir/$index"
printf '%s\n' due >"$schedule_file"
: >"$systemctl_log"
output=$(run_helper "$profile" disable)
assert_output "$output" \
  "host=$profile	action=disable	backup_unit=$desktop	desktop_enabled=false	server_enabled=false	index_enabled=false"
assert_log ignored \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index" \
  "disable --now $desktop" \
  "disable --now $server" \
  "disable --now $index" \
  "is-enabled --quiet $desktop" \
  "is-enabled --quiet $server" \
  "is-enabled --quiet $index" \
  "is-active --quiet $desktop" \
  "is-active --quiet $server" \
  "is-active --quiet $index"

: >"$systemctl_log"
printf '%s\n' unexpected >"$schedule_file"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Malformed schedule output was accepted.' >&2
  exit 1
fi
[ ! -s "$systemctl_log" ]

mkdir -p "$boxup_state/$profile"
: >"$boxup_state/$profile/requires-live-validation"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Automation was enabled before live validation.' >&2
  exit 1
fi
rm -f "$boxup_state/$profile/requires-live-validation"

ln -s missing "$boxup_state/$profile/requires-live-validation"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Automation accepted a dangling validation-marker symlink.' >&2
  exit 1
fi
rm -f "$boxup_state/$profile/requires-live-validation"

rm -f "$boxup_state/$profile/last-success.json"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Automation was enabled before a successful manual backup.' >&2
  exit 1
fi
: >"$boxup_state/$profile/last-success.json"

printf '%s\n' due >"$schedule_file"
rm -f "$state_dir"/* "$active_dir"/* "$BOXUP_TEST_FAIL_MARKER"
: >"$state_dir/$server"
: >"$active_dir/$server"
export BOXUP_TEST_FAIL_ONCE="enable --now $index"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Injected enable failure was not reported.' >&2
  exit 1
fi
unset BOXUP_TEST_FAIL_ONCE
[ ! -e "$state_dir/$desktop" ]
[ -e "$state_dir/$server" ]
[ ! -e "$state_dir/$index" ]
[ ! -e "$active_dir/$desktop" ]
[ -e "$active_dir/$server" ]
[ ! -e "$active_dir/$index" ]

printf '%s\n' due >"$schedule_file"
chmod 0620 "$config"
if run_helper "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'Writable profile was accepted.' >&2
  exit 1
fi
chmod 0600 "$config"

if run_helper '../unsafe' enable >/dev/null 2>&1; then
  printf '%s\n' 'Unsafe profile name was accepted.' >&2
  exit 1
fi

ln -s "$config" "$config_dir/symlink.toml"
if run_helper symlink enable >/dev/null 2>&1; then
  printf '%s\n' 'Symlinked profile was accepted.' >&2
  exit 1
fi

if env -u BOXUP_AUTOMATION_TEST_ONLY \
  BOXUP_AUTOMATION_SYSTEMCTL="$fake_systemctl" \
  sh "$helper" "$profile" enable >/dev/null 2>&1; then
  printf '%s\n' 'A test override was accepted without the test marker.' >&2
  exit 1
fi

if grep -Eq 'boxup-(maintenance|check)@' "$systemctl_log"; then
  printf '%s\n' 'The helper invoked a maintenance or check unit.' >&2
  exit 1
fi
printf '%s\n' 'setup-automation tests passed'
