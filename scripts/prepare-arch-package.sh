#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${BOXUP_OUTPUT_DIR:-"$project_dir/dist"}
BOXUP_OUTPUT_DIR="$output_dir" sh "$project_dir/scripts/make-source-archive.sh" >/dev/null
archive="$output_dir/boxup-0.1.0.tar.gz"
(cd "$output_dir" && sha256sum -c boxup-0.1.0.tar.gz.sha256 >/dev/null)
checksum=$(sha256sum "$archive" | cut -d ' ' -f 1)

cp "$archive" "$project_dir/packaging/arch/boxup-0.1.0.tar.gz"
temporary=$(mktemp "${TMPDIR:-/tmp}/boxup-pkgbuild.XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
sed "s/^sha256sums=.*/sha256sums=('$checksum')/" \
  "$project_dir/packaging/arch/PKGBUILD" >"$temporary"
mv "$temporary" "$project_dir/packaging/arch/PKGBUILD"
chmod 0644 "$project_dir/packaging/arch/PKGBUILD"
trap - EXIT HUP INT TERM
printf 'Prepared Arch source with SHA-256 %s\n' "$checksum"
