# Restore And Recovery

## Recovery Material

For each host, keep these items separately from its Borg repository:

- The exact versioned Boxup profile, with secret values still represented only by paths.
- The Borg repokey export.
- The passphrase and, if used, separate maintenance SSH key.
- The SSH private key and pinned known-hosts entry.
- A list of source filesystem layouts and Docker application recovery steps.

Use a separate repository and key for every host. Verify key export immediately
after repository initialization and whenever encryption settings change. Never
print a passphrase, private key, webhook URL, or decrypted recovery material in
logs or tickets.

## Normal Restore

1. Install Borg 1.4 and Boxup on an isolated recovery machine.
2. Restore the profile, `/etc/boxup/HOST.passphrase`, SSH keys, known-hosts file,
   and repokey export with mode 0600 where secret.
3. Verify the SSH host key out of band.
4. Run `boxup --config /etc/boxup/HOST.toml snapshots --live` against the live
   repository.
5. Refresh the browsing cache with
   `boxup --config /etc/boxup/HOST.toml index refresh`; treat it only as a cache.
6. Search or browse, then restore to a new absolute destination.

```sh
boxup search 'important-name' --all-snapshots
boxup --config /etc/boxup/HOST.toml ls HOST-ARCHIVE home/user/Documents --live
pkexec /usr/bin/boxup --config /etc/boxup/HOST.toml restore \
  HOST-ARCHIVE /home/user/Documents/project --to /recovery/project
```

Selections may have one leading `/`, which is removed to obtain the exact
archive-relative path. `..`, empty or dot components, archive root, Borg
selector prefixes, control characters, protected config/credential/state paths,
excessive file counts/bytes, and special files are rejected. Wildcards remain
literal because extraction uses explicit include `pp:` patterns. The
selected archive name, Borg ID, and all preflight items come from live Borg, not
SQLite. Boxup compares every extracted path/type, regular-file size, and symlink
target to that live manifest, then revalidates the Borg ID before publication.

The destination must be absent or an empty non-symlink directory. Every existing
parent component is checked with `symlink_metadata`. Extraction occurs under the
configured staging root, which must share a filesystem with the destination;
publication then uses Linux `renameat2(RENAME_NOREPLACE)`. Boxup does not follow
extracted symlinks while inspecting or publishing.

The configured Docker staging path is protected as a restore destination but is
not denied as an archive selection. This permits restoring staged volume data to
a different safe recovery destination.

The generated `STATE/inventory/ARCHIVE.json` follows the same rule: it may be
selected from an archive and restored to a safe external destination, while the
live Boxup state directory remains protected as a restore destination.

## Overwrite Restore

Overwrite is an emergency operation, not a normal recovery shortcut. It is only
available as:

```sh
boxup --config /etc/boxup/HOST.toml restore \
  HOST-ARCHIVE PATH... --to / --overwrite --sudo
```

It requires a `/etc/boxup/*.toml` system profile, the fixed `boxup-root` helper,
root, an attached TTY, live repository validation, a displayed dry-run count,
and the exact typed phrase `RESTORE HOST-ARCHIVE TO /`. Staging must be on the
root filesystem. Existing destination symlink components are refused; leaf
symlinks are unlinked rather than followed. Cross-filesystem publication is
refused instead of silently becoming a non-atomic copy.

The root merge pins destination directories with file descriptors and opens each
existing child directory with `O_NOFOLLOW`. Leaf replacement uses `renameat`, so
it replaces a symlink itself rather than traversing its target.

Prefer restoring into a new root filesystem and switching boot targets. Root
merge cannot be transactional and may leave a partially restored system after
power loss, disk failure, or a late permission error.

## Mount

Install `pyfuse3` or `llfuse` plus `fusermount`, then use an existing empty,
non-symlink directory:

```sh
mkdir /tmp/boxup-mount
pkexec /usr/bin/boxup --config /etc/boxup/HOST.toml mount \
  HOST-ARCHIVE /tmp/boxup-mount
pkexec /usr/bin/boxup --config /etc/boxup/HOST.toml umount /tmp/boxup-mount
```

Do not treat a successful mount or list as a restore test.

## Docker Recovery

The Docker staging archive contains `mounts/CONTAINER_ID/N`, configured service
data under `services/POSITION`, and PostgreSQL SQL dumps. Consult the inventory
JSON to map every original mount position back to container, destination, type,
source, and Compose project. Map service positions using the ordered
`docker.service_paths` in the archived profile. Restore Compose definitions and
secrets separately, stop the corresponding containers and services, restore
volume and service data, then use SQL dumps for PostgreSQL logical recovery where
possible.

Stopped selected containers receive one already-quiescent copy when no configured
service is active. Running selected containers and configured service paths
receive online and final `rsync -aHAXS` copies whenever the workflow quiesces an
active selected container or service. Only mount or service sources whose
staging completed are excluded from ordinary source backup; unlisted mounts
remain covered. The recovery journal records prior-active container IDs and
service names; recovery restarts only that set and retains the journal until all
are verified running/active. `pg_dumpall` adds logical consistency only when it
completed successfully with the configured role and was tested. No
application-consistency claim applies to other databases or to workloads
excluded from quiescing.

## Rehearsal

At least quarterly and after meaningful policy changes:

1. Restore representative files into a temporary empty directory.
2. Compare hashes, modes, ownership, times, and symlink targets.
3. Restore a PostgreSQL dump into a disposable matching major version.
4. Recreate one Compose project from restored configuration and data.
5. Verify repokey/passphrase recovery on a machine without the original cache.
6. Record the archive, commands, duration, and outcome without secrets.

Metadata checks are suitable for routine scheduling. Run `boxup check
--verify-data` deliberately because a full data verification can take substantial
time and repository bandwidth.

The index is only usable after a transactional refresh records repository ID,
location, completion, and time. `snapshots` and `ls` fall back to live Borg when
it is incomplete, mismatched, or stale; use `--live` to force live reads.
