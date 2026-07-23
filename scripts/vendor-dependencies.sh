#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

stage=$(mktemp -d "$project_dir/.boxup-vendor.XXXXXX")
config="$project_dir/.cargo/config.toml"
config_backup="$stage/original-config.toml"
restore_config() {
  if [ -f "$config_backup" ] && [ ! -e "$config" ]; then
    mkdir -p "$project_dir/.cargo"
    mv "$config_backup" "$config"
  fi
  rm -rf "$stage"
}
trap restore_config EXIT HUP INT TERM

if [ -f "$config" ]; then
  mv "$config" "$config_backup"
fi
if [ "${BOXUP_VENDOR_OFFLINE:-0}" != 1 ]; then
  cargo fetch --locked
fi
BOXUP_VENDOR_DIR="$stage/vendor" BOXUP_CARGO_CONFIG="$stage/config.toml" \
  BOXUP_NOTICES="$stage/vendor/THIRD_PARTY_NOTICES" \
  python3 scripts/vendor-linux-dependencies.py

rm -rf "$project_dir/vendor"
mv "$stage/vendor" "$project_dir/vendor"
mkdir -p "$project_dir/.cargo"
mv "$stage/config.toml" "$config"
rm -f "$config_backup"
trap - EXIT HUP INT TERM
rm -rf "$stage"
printf '%s\n' 'Vendored locked Linux dependencies into vendor/ and .cargo/config.toml.'
