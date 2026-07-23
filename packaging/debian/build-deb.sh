#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
sh "$project_dir/scripts/make-source-archive.sh" >/dev/null
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM
source_dir="$work_dir/boxup-0.1.0"
tar -C "$work_dir" -xzf "$project_dir/dist/boxup-0.1.0.tar.gz"
cp -a "$project_dir/packaging/debian/debian" "$source_dir/debian"
cp "$project_dir/dist/boxup-0.1.0.tar.gz" "$work_dir/boxup_0.1.0.orig.tar.gz"
(cd "$source_dir" && dpkg-buildpackage --build=binary --no-sign)
mkdir -p "$project_dir/dist"
cp "$work_dir"/boxup_0.1.0-1_*.deb "$project_dir/dist/"
