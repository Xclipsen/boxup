#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${BOXUP_OUTPUT_DIR:-"$project_dir/dist"}
archive="$output_dir/boxup-0.1.0.tar.gz"
arch_archive="$project_dir/packaging/arch/boxup-0.1.0.tar.gz"
pkgbuild="$project_dir/packaging/arch/PKGBUILD"
debian_control="$project_dir/packaging/debian/debian/control"
debian_rules="$project_dir/packaging/debian/debian/rules"
setup="$project_dir/scripts/setup-profile.sh"

(cd "$output_dir" && sha256sum -c boxup-0.1.0.tar.gz.sha256 >/dev/null)
checksum=$(sha256sum "$archive" | cut -d ' ' -f 1)
cmp -s "$archive" "$arch_archive"
grep -Fqx "sha256sums=('$checksum')" "$pkgbuild"
if grep -Fq 'SKIP' "$pkgbuild"; then
  printf '%s\n' 'PKGBUILD may not skip source checksum verification.' >&2
  exit 1
fi
python3 - "$project_dir" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
notice = (root / "vendor" / "THIRD_PARTY_NOTICES").read_bytes()
prefixes = ("COPYING", "COPYRIGHT", "LICENSE", "NOTICE", "UNLICENSE")
missing = []
for package in sorted((root / "vendor").iterdir()):
    if not package.is_dir():
        continue
    for document in sorted(package.rglob("*")):
        if not document.is_file() or not document.name.upper().startswith(prefixes):
            continue
        relative = document.relative_to(package).as_posix()
        contents = document.read_bytes()
        expected = f"----- BEGIN {relative} -----\n".encode("utf-8") + contents
        if not contents.endswith(b"\n"):
            expected += b"\n"
        expected += f"----- END {relative} -----\n".encode("utf-8")
        if expected not in notice:
            missing.append(f"{package.name}/{relative}")
if missing:
    raise SystemExit("Missing third-party legal documents: " + ", ".join(missing))
PY
grep -Fq "'borg>=1.4'" "$pkgbuild"
grep -Fq "'polkit'" "$pkgbuild"
grep -Fq "'systemd'" "$pkgbuild"
grep -Fq "'python-pyfuse3'" "$pkgbuild"
grep -Fq "'sqlite'" "$pkgbuild"
grep -Fq 'install -Dm755 scripts/setup-profile.sh' "$pkgbuild"
grep -Fq 'vendor/THIRD_PARTY_NOTICES' "$pkgbuild"
grep -Fq 'install -Dm644 AGENTS.md' "$pkgbuild"
grep -Fq 'sh scripts/check-rust-version.sh' "$pkgbuild"
grep -Fq 'borgbackup (>= 1.4)' "$debian_control"
grep -Fq 'pkexec, polkitd' "$debian_control"
grep -Fq 'rsync, systemd' "$debian_control"
grep -Fq 'python3-pyfuse3' "$debian_control"
grep -Fq 'libsqlite3-dev' "$debian_control"
grep -Fqx 'override_dh_clean:' "$debian_rules"
grep -Fqx '	dh_clean -XCargo.toml.orig' "$debian_rules"
grep -Fqx 'override_dh_installsystemd:' "$debian_rules"
grep -Fqx '	dh_installsystemd --no-enable --no-start' "$debian_rules"
grep -Fq 'install -Dm755 scripts/setup-profile.sh' "$debian_rules"
grep -Fq 'vendor/THIRD_PARTY_NOTICES' "$debian_rules"
grep -Fq 'install -Dm644 AGENTS.md' "$debian_rules"
grep -Fq '	sh scripts/check-rust-version.sh' "$debian_rules"

grep -Fq "system_config=\"/etc/boxup/\$profile.toml\"" "$setup"
grep -Fq "passphrase=\"/etc/boxup/\$profile.passphrase\"" "$setup"
grep -Fq "index=\"\$index_dir/index.sqlite3\"" "$setup"
grep -Fq "\"/var/lib/boxup-restore/\$profile\"" "$setup"
grep -Fq "\"/var/lib/boxup-docker/\$profile\"" "$setup"
grep -Fq "config validate \\" "$setup"
grep -Fq -- "--system-profile \"\$system_config\"" "$setup"
grep -Fq "boxup-root --config \"\$system_config\" prepare" "$setup"
grep -Fq "boxup-root --config \"\$system_config\" print-schedule" "$setup"
grep -Fq "systemd-analyze calendar \"\$calendar\"" "$setup"

grep -Fq 'org.boxup.run-fixed-helper' "$project_dir/packaging/polkit/org.boxup.policy"
grep -Fq 'org.boxup.setup-profile' "$project_dir/packaging/polkit/org.boxup.policy"

for example in "$project_dir"/examples/*.toml; do
  grep -Fq 'passphrase_file = "/etc/boxup/' "$example"
  grep -Fq '"pp:var/lib/docker/overlay2"' "$example"
  grep -Fq '"pp:var/lib/docker/image"' "$example"
  grep -Fq '"pp:var/lib/docker/buildkit"' "$example"
  if grep -Eq '"(/|pp:)?var/lib/docker"' "$example"; then
    printf 'Unsafe blanket Docker exclusion in %s.\n' "$example" >&2
    exit 1
  fi
done
if grep -Fq '/run/credentials' "$project_dir"/README.md "$project_dir"/docs/*.md "$project_dir"/examples/*.toml; then
  printf '%s\n' 'Runtime credential paths remain in installed guidance.' >&2
  exit 1
fi
grep -Fq 'borg-1.4 serve --append-only --restrict-to-repository' "$project_dir/README.md"
if grep -Fq -- '--restrict-to-path' "$project_dir/README.md"; then
  printf '%s\n' 'Borg forced-command examples use the obsolete repository restriction option.' >&2
  exit 1
fi
