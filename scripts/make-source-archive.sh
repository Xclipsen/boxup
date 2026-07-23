#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=0.1.0
epoch=${SOURCE_DATE_EPOCH:-0}
output_dir=${BOXUP_OUTPUT_DIR:-"$project_dir/dist"}
archive="$output_dir/boxup-$version.tar.gz"
checksum_file="$archive.sha256"

case "$epoch" in
  ''|*[!0-9]*) printf '%s\n' 'SOURCE_DATE_EPOCH must be a non-negative integer.' >&2; exit 1 ;;
esac
export LC_ALL=C TZ=UTC
umask 022

if [ ! -d "$project_dir/vendor" ] || [ ! -s "$project_dir/.cargo/config.toml" ]; then
  printf '%s\n' 'Run scripts/vendor-dependencies.sh before creating an offline source archive.' >&2
  exit 1
fi
if ! git -C "$project_dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf '%s\n' 'Source archives must be created from a Git working tree.' >&2
  exit 1
fi
mkdir -p "$output_dir"
output_dir=$(CDPATH='' cd -- "$output_dir" && pwd)
case "$output_dir" in
  "$project_dir"/*)
    [ "$output_dir" = "$project_dir/dist" ] || {
      printf '%s\n' 'BOXUP_OUTPUT_DIR must be dist/ or outside the source tree.' >&2
      exit 1
    }
    ;;
esac
archive="$output_dir/boxup-$version.tar.gz"
checksum_file="$archive.sha256"
temporary_tar=$(mktemp "$output_dir/.boxup-source-tar.XXXXXX")
temporary_archive=$(mktemp "$output_dir/.boxup-source-archive.XXXXXX")
temporary_checksum=$(mktemp "$output_dir/.boxup-source-checksum.XXXXXX")
temporary_allowlist=$(mktemp "$output_dir/.boxup-source-allowlist.XXXXXX")
temporary_files=$(mktemp "$output_dir/.boxup-source-files.XXXXXX")
trap 'rm -f "$temporary_tar" "$temporary_archive" "$temporary_checksum" "$temporary_allowlist" "$temporary_files"' EXIT HUP INT TERM

cat >"$temporary_allowlist" <<'EOF'
.cargo/config.toml
.gitattributes
.gitignore
Cargo.lock
Cargo.toml
AGENTS.md
LICENSE
README.md
SECURITY.md
docs/RESTORE.md
examples/desktop.toml
examples/ubuntu-docker-vps.toml
packaging/arch/boxup.install
packaging/debian/build-deb.sh
packaging/debian/debian/changelog
packaging/debian/debian/control
packaging/debian/debian/copyright
packaging/debian/debian/postrm
packaging/debian/debian/prerm
packaging/debian/debian/rules
packaging/debian/debian/source/format
packaging/polkit/org.boxup.policy
packaging/systemd/boxup-backup-desktop@.timer
packaging/systemd/boxup-backup-due@.service
packaging/systemd/boxup-backup-now@.service
packaging/systemd/boxup-backup-server@.timer
packaging/systemd/boxup-check@.service
packaging/systemd/boxup-check@.timer
packaging/systemd/boxup-maintenance@.service
packaging/systemd/boxup-maintenance@.timer
scripts/bootstrap.sh
scripts/check-rust-version.sh
scripts/make-source-archive.sh
scripts/prepare-arch-package.sh
scripts/setup-profile.sh
scripts/vendor-dependencies.sh
scripts/vendor-linux-dependencies.py
scripts/verify-packaging.sh
scripts/verify-source-archive.sh
scripts/verify-units.sh
src/backend.rs
src/bin/boxup-root.rs
src/bin/boxup.rs
src/borg.rs
src/config.rs
src/domain.rs
src/index.rs
src/jobs.rs
src/lib.rs
src/restore.rs
src/tui.rs
tests/docker_workflow.rs
tests/fake_borg.rs
tests/fixtures/archive-list.jsonl
tests/fixtures/create.json
tests/fixtures/diff.jsonl
tests/fixtures/repository-list.json
EOF

while IFS= read -r path; do
  if ! git -C "$project_dir" ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
    printf 'Source archive allowlist entry is not tracked: %s\n' "$path" >&2
    exit 1
  fi
  printf '%s\0' "$path" >>"$temporary_files"
done <"$temporary_allowlist"
if ! git -C "$project_dir" ls-files --error-unmatch -- vendor >/dev/null 2>&1; then
  printf '%s\n' 'Vendored dependencies are not tracked.' >&2
  exit 1
fi
git -C "$project_dir" ls-files -z -- vendor >>"$temporary_files"

tar --sort=name --format=posix --mtime="@$epoch" --owner=0 --group=0 \
  --numeric-owner --pax-option=delete=atime,delete=ctime \
  --null --verbatim-files-from --no-recursion \
  --transform="s,^,boxup-$version/," -C "$project_dir" \
  -cf "$temporary_tar" --files-from="$temporary_files"
gzip -n -9 <"$temporary_tar" >"$temporary_archive"
checksum=$(sha256sum "$temporary_archive" | cut -d ' ' -f 1)
printf '%s  %s\n' "$checksum" "$(basename "$archive")" >"$temporary_checksum"
mv "$temporary_archive" "$archive"
mv "$temporary_checksum" "$checksum_file"
rm -f "$temporary_tar"
trap - EXIT HUP INT TERM
printf 'sha256sums=(%s)\n' "'$checksum'"
printf 'Created %s and %s.\n' "$archive" "$checksum_file"
