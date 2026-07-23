#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/boxup-archive.XXXXXX")
secret_dir=$(mktemp -d "$project_dir/source-archive-private.XXXXXX")
trap 'rm -rf "$work_dir" "$secret_dir"' EXIT HUP INT TERM
first_dir="$work_dir/first"
second_dir="$work_dir/second"
extract_dir="$work_dir/extracted"
mkdir -p "$first_dir" "$second_dir" "$extract_dir"
archive="$first_dir/boxup-0.1.0.tar.gz"
listing="$work_dir/listing"
secret_relative=${secret_dir#"$project_dir"/}

printf '%s\n' 'source archive credential marker' >"$secret_dir/credentials.key"
printf '%s\n' 'source archive environment marker' >"$secret_dir/.env"
printf '%s\n' 'source archive database marker' >"$secret_dir/state.sqlite3"
printf '%s\n' 'source archive recovery marker' >"$secret_dir/recovery-bundle.age"
printf '%s\n' 'source archive private operations marker' >"$secret_dir/private-ops.conf"
for ignored in credentials.key .env state.sqlite3 recovery-bundle.age; do
  git -C "$project_dir" check-ignore -q -- "$secret_relative/$ignored"
done

SOURCE_DATE_EPOCH=0 BOXUP_OUTPUT_DIR="$first_dir" \
  sh "$project_dir/scripts/make-source-archive.sh" >/dev/null
SOURCE_DATE_EPOCH=0 BOXUP_OUTPUT_DIR="$second_dir" \
  sh "$project_dir/scripts/make-source-archive.sh" >/dev/null
cmp -s "$archive" "$second_dir/boxup-0.1.0.tar.gz" || {
  printf '%s\n' 'Source archive is not reproducible.' >&2
  exit 1
}
cmp -s "$archive.sha256" "$second_dir/boxup-0.1.0.tar.gz.sha256"
(cd "$first_dir" && sha256sum -c boxup-0.1.0.tar.gz.sha256)

tar -tzf "$archive" >"$listing"
if grep -Eq '^boxup-0\.1\.0/(target|dist|scripts/__pycache__)/|\.py[co]$|^boxup-0\.1\.0/packaging/arch/(pkg|src)/' "$listing"; then
  printf '%s\n' 'Source archive contains generated build artifacts.' >&2
  exit 1
fi
if grep -Fq "boxup-0.1.0/$secret_relative/" "$listing"; then
  printf '%s\n' 'Source archive contains an ignored or untracked private file.' >&2
  exit 1
fi
for forbidden in plan.md .github/workflows/ci.yml packaging/arch/PKGBUILD; do
  if grep -Fqx "boxup-0.1.0/$forbidden" "$listing"; then
    printf 'Source archive contains non-release file: %s\n' "$forbidden" >&2
    exit 1
  fi
done
grep -Fqx 'boxup-0.1.0/Cargo.lock' "$listing"
grep -Fqx 'boxup-0.1.0/.cargo/config.toml' "$listing"
grep -Fqx 'boxup-0.1.0/AGENTS.md' "$listing"
grep -Fqx 'boxup-0.1.0/SECURITY.md' "$listing"
grep -Eq '^boxup-0\.1\.0/vendor/[^/]+/Cargo\.toml$' "$listing"
tar -C "$extract_dir" -xzf "$archive"
mkdir "$work_dir/cargo-home" "$work_dir/target"
CARGO_HOME="$work_dir/cargo-home" CARGO_TARGET_DIR="$work_dir/target" \
  CARGO_NET_OFFLINE=true cargo check --manifest-path "$extract_dir/boxup-0.1.0/Cargo.toml" \
  --frozen --offline
