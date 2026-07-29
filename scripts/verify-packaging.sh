#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${BOXUP_OUTPUT_DIR:-"$project_dir/dist"}
archive="$output_dir/boxup-0.2.0.tar.gz"
arch_archive="$project_dir/packaging/arch/boxup-0.2.0.tar.gz"
pkgbuild="$project_dir/packaging/arch/PKGBUILD"
debian_control="$project_dir/packaging/debian/debian/control"
debian_rules="$project_dir/packaging/debian/debian/rules"
setup="$project_dir/scripts/setup-profile.sh"
automation="$project_dir/scripts/setup-automation.sh"
fresh_dir=$(mktemp -d "${TMPDIR:-/tmp}/boxup-packaging.XXXXXX")
trap 'rm -rf "$fresh_dir"' EXIT HUP INT TERM

(cd "$output_dir" && sha256sum -c boxup-0.2.0.tar.gz.sha256 >/dev/null)
checksum=$(sha256sum "$archive" | cut -d ' ' -f 1)
cmp -s "$archive" "$arch_archive"
SOURCE_DATE_EPOCH=0 BOXUP_OUTPUT_DIR="$fresh_dir" \
  sh "$project_dir/scripts/make-source-archive.sh" >/dev/null
cmp -s "$archive" "$fresh_dir/boxup-0.2.0.tar.gz" || {
  printf '%s\n' 'Prepared source archive is stale relative to the current worktree.' >&2
  exit 1
}
tar -tzf "$archive" \
  boxup-0.2.0/scripts/setup-automation.sh \
  boxup-0.2.0/tests/setup-automation.sh >/dev/null
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
grep -Fq "'bash-completion'" "$pkgbuild"
grep -Fq "'polkit'" "$pkgbuild"
grep -Fq "'sudo'" "$pkgbuild"
grep -Fq "'systemd'" "$pkgbuild"
grep -Fq "'python-pyfuse3'" "$pkgbuild"
grep -Fq "'libnotify'" "$pkgbuild"
grep -Fq "'sqlite'" "$pkgbuild"
grep -Fq 'install -Dm755 scripts/setup-profile.sh' "$pkgbuild"
grep -Fq 'install -Dm755 scripts/setup-backup-sudo.sh' "$pkgbuild"
grep -Fq 'install -Dm755 scripts/setup-automation.sh' "$pkgbuild"
grep -Fq 'sh tests/setup-automation.sh' "$pkgbuild"
grep -Fq 'completions/boxup.bash' "$pkgbuild"
grep -Fq 'completions/_boxup' "$pkgbuild"
grep -Fq 'completions/boxup.fish' "$pkgbuild"
grep -Fq 'vendor/THIRD_PARTY_NOTICES' "$pkgbuild"
grep -Fq 'install -Dm644 AGENTS.md' "$pkgbuild"
grep -Fq 'sh scripts/check-rust-version.sh' "$pkgbuild"
grep -Fq 'borgbackup (>= 1.4)' "$debian_control"
grep -Fq 'bash-completion' "$debian_control"
grep -Fq 'pkexec, polkitd' "$debian_control"
grep -Fq 'sudo' "$debian_control"
grep -Fq 'rsync, systemd' "$debian_control"
grep -Fq 'python3-pyfuse3' "$debian_control"
grep -Fq 'libnotify-bin' "$debian_control"
grep -Fq 'libsqlite3-dev' "$debian_control"
grep -Fqx 'override_dh_clean:' "$debian_rules"
grep -Fqx '	dh_clean -XCargo.toml.orig' "$debian_rules"
grep -Fqx 'override_dh_installsystemd:' "$debian_rules"
grep -Fqx '	dh_installsystemd --no-enable --no-start' "$debian_rules"
grep -Fq 'install -Dm755 scripts/setup-profile.sh' "$debian_rules"
grep -Fq 'install -Dm755 scripts/setup-backup-sudo.sh' "$debian_rules"
grep -Fq 'install -Dm755 scripts/setup-automation.sh' "$debian_rules"
grep -Fq 'sh tests/setup-automation.sh' "$debian_rules"
grep -Fq 'completions/boxup.bash' "$debian_rules"
grep -Fq 'completions/_boxup' "$debian_rules"
grep -Fq 'usr/share/zsh/vendor-completions/_boxup' "$debian_rules"
grep -Fq 'completions/boxup.fish' "$debian_rules"
grep -Fq 'vendor/THIRD_PARTY_NOTICES' "$debian_rules"
grep -Fq 'boxup-notify@.service' "$debian_rules"
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
grep -Fq 'setup-backup-sudo "$profile" "$browse_user"' "$setup"
grep -Fq "boxup-root --config \"\$system_config\" print-schedule" "$setup"
grep -Fq "systemd-analyze calendar \"\$calendar\"" "$setup"

grep -Fq 'org.boxup.run-fixed-helper' "$project_dir/packaging/polkit/org.boxup.policy"
grep -Fq 'org.boxup.setup-profile' "$project_dir/packaging/polkit/org.boxup.policy"
grep -Fq 'org.boxup.setup-automation' "$project_dir/packaging/polkit/org.boxup.policy"
grep -Fq '<allow_active>auth_admin</allow_active>' "$project_dir/packaging/polkit/org.boxup.policy"
grep -Fq '<annotate key="org.freedesktop.policykit.exec.path">/usr/lib/boxup/setup-automation</annotate>' \
  "$project_dir/packaging/polkit/org.boxup.policy"

grep -Fq '"$boxup_root_path" --config "$config" print-schedule' "$automation"
grep -Fq '"$systemctl_path" daemon-reload' "$automation"
grep -Fq '"$systemctl_path" disable --now "$other_backup_timer"' "$automation"
grep -Fq '"$systemctl_path" enable --now "$backup_timer"' "$automation"
grep -Fq '"$systemctl_path" enable --now "$index_timer"' "$automation"
grep -Fq 'boxup-backup-desktop@$profile.timer' "$automation"
grep -Fq 'boxup-backup-server@$profile.timer' "$automation"
grep -Fq 'boxup-index@$profile.timer' "$automation"
if grep -Eq 'boxup-(maintenance|check)@' "$automation"; then
  printf '%s\n' 'Automation helper may not manage maintenance or check units.' >&2
  exit 1
fi
sh "$project_dir/tests/setup-automation.sh"

sudo_setup="$project_dir/scripts/setup-backup-sudo.sh"
grep -Fq 'NOPASSWD: /usr/lib/boxup/boxup-root --config /etc/boxup/%s.toml backup --progress-json' "$sudo_setup"
grep -Fq 'visudo" -cf' "$sudo_setup"
grep -Fq 'reserved sudoers principal ALL' "$sudo_setup"
if grep -Eq 'NOPASSWD:.*(/usr/bin/boxup|boxup-root.*(init|maintenance|restore|check))' "$sudo_setup"; then
  printf '%s\n' 'Passwordless sudo rule is broader than foreground backup.' >&2
  exit 1
fi

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
