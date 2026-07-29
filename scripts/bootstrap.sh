#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

if command -v pacman >/dev/null 2>&1; then
  pkexec pacman -S --needed acl base-devel bash-completion borg rust cargo curl fuse3 libnotify openssh polkit python-pyfuse3 rsync sudo
  sh scripts/check-rust-version.sh
  cargo fetch --locked
  sh scripts/vendor-dependencies.sh
  sh scripts/prepare-arch-package.sh
  (cd packaging/arch && makepkg --cleanbuild --force)
  package=$(find packaging/arch -maxdepth 1 -name 'boxup-0.2.0-1-*.pkg.tar.zst' -print -quit)
  [ -n "$package" ]
  pkexec pacman -U "$project_dir/$package"
elif command -v apt-get >/dev/null 2>&1; then
  pkexec apt-get install acl bash-completion borgbackup build-essential cargo curl debhelper fuse3 libnotify-bin openssh-client pkexec polkitd python3-pyfuse3 rsync rustc sudo
  sh scripts/check-rust-version.sh
  cargo fetch --locked
  sh scripts/vendor-dependencies.sh
  ./packaging/debian/build-deb.sh
  package=$(find dist -maxdepth 1 -name 'boxup_0.2.0-1_*.deb' -print -quit)
  [ -n "$package" ]
  pkexec apt-get install "$project_dir/$package"
else
  echo 'Unsupported distribution; use the Arch or Debian packaging files.' >&2
  exit 1
fi

echo 'Package installed; no config, credentials, repository, profile unit, or timer was enabled.'
