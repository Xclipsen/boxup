#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
required=$(sed -n 's/^rust-version = "\([0-9][0-9.]*\)"$/\1/p' "$project_dir/Cargo.toml")
case "$required" in
  ''|.*|*.|*.*.*|*[!0-9.]*)
    printf '%s\n' 'Cargo.toml must contain one major.minor rust-version.' >&2
    exit 1
    ;;
esac
required_major=${required%%.*}
required_minor=${required#*.}
if [ "$required_major" -lt 1 ] || \
  { [ "$required_major" -eq 1 ] && [ "$required_minor" -lt 85 ]; }; then
  printf 'Cargo.toml rust-version must be Rust 1.85 or newer; found %s.\n' "$required" >&2
  exit 1
fi

version_line=$(rustc --version)
version=${version_line#rustc }
version=${version%% *}
major=${version%%.*}
remainder=${version#*.}
minor=${remainder%%.*}
case "$major:$minor" in
  *[!0-9:]*|:*) printf 'Unable to parse rustc version: %s\n' "$version_line" >&2; exit 1 ;;
esac
if [ "$major" -lt "$required_major" ] || \
  { [ "$major" -eq "$required_major" ] && [ "$minor" -lt "$required_minor" ]; }; then
  printf 'Rust %s or newer is required; found %s.\n' "$required" "$version" >&2
  exit 1
fi
