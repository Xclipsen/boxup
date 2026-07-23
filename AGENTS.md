# Repository Safety Rules

- Read `README.md`, `docs/RESTORE.md`, and the relevant implementation before
  changing behavior. Keep documentation aligned with user-visible changes.
- Never run Borg against a non-temporary repository while developing or testing.
- Never initialize, prune, compact, unlock, delete, or rewrite a real repository
  without explicit authorization for that exact target and operation.
- Never access remote hosts or reuse existing backup configuration, credentials,
  repositories, state, or recovery files during development tests.
- Tests must run without network and root. A real-Borg test may use only a fresh
  `tempfile` repository and must skip when Borg 1.4 is unavailable.
- Never place a passphrase, private key, webhook URL, or container environment in
  argv, config examples, fixtures, logs, snapshots, or assertion messages.
- Preserve the shell-free Borg/SSH/curl/docker/rsync execution model.
- The SQLite index is untrusted for restore, prune, compact, check, initialization,
  or any destructive decision.
- Keep restore default-deny: live snapshot validation, normalized literal paths,
  protected-path denial, limits, special-file checks, isolated staging, no
  overwrite by default, explicit confirmation for original-path replacement,
  and no symlink traversal.
- Root-helper operations must stay fixed and profiles must remain constrained to
  canonical, root-owned, non-writable `/etc/boxup/*.toml` files.
- Packaging may install immutable `/usr` files only. Setup, credentials, state,
  repositories, and timer enablement remain separate explicit actions.
- Source archives must contain only the reviewed tracked allowlist and vendored
  dependencies. Never package local profiles, operations notes, credentials,
  databases, recovery material, or unexpected worktree files.
- Do not claim application consistency without a tested application-aware dump or
  quiesce procedure and a successful restore rehearsal.

# Fresh-System Adoption And Recovery Runbook

Use this runbook when a user wants to install Boxup on another machine, reconnect
an existing repository after reinstalling the operating system, or restore
selected files into a fresh system. Treat these as different operations:

- **Selective recovery** reads an existing repository and publishes selected
  files into a separate destination. It does not initialize a repository or
  enable backup, maintenance, or check timers.
- **Replacement-host adoption** makes the new installation the writer for an
  existing host profile and repository. The old writer must be disabled first.
- **New deployment** creates a new host profile, credentials, and an empty
  repository namespace. It must not reuse another host's repository or keys.

Do not infer which operation the user wants. Ask the questions below before
writing a profile, installing credentials, contacting a remote host, running
`boxup init`, or enabling a timer. Ask for paths and identifiers, never secret
contents. Do not ask the user to paste a private key, passphrase, recovery phrase,
webhook URL, or decrypted recovery bundle into chat.

## Required Discovery Questions

Record the answers without recording secrets:

1. Is this selective recovery, replacement-host adoption, or a new deployment?
2. Which distribution and release is the target using: Arch/Omarchy,
   Debian/Ubuntu, or something else? What is the intended local browse user?
3. What is the Boxup profile/host ID? For an existing repository, what exact
   `host.id` and archive prefix were used previously?
4. Does the Borg repository already exist? Obtain the exact repository URI from
   the recovered profile or recovery documentation; do not reconstruct it from
   memory.
5. What is the exact Storage Box SSH endpoint: hostname, username, and port? Is
   `storagebox` merely a local `~/.ssh/config` alias? If an alias is supplied,
   resolve it locally with `ssh -G ALIAS` and record only the resolved hostname,
   user, port, and identity-file paths. Do not assume that the alias exists on
   the fresh system.
6. What is the repository-relative path on the Storage Box, and which remote Borg
   executable is available (`borg`, `borg-1.4`, or an absolute path)? Do not
   confuse the SSH account's home-relative `./...` path with an absolute server
   filesystem path.
7. Which local file contains the routine repository SSH private key? Which file,
   if any, contains the separate maintenance key? Ask only for file paths and key
   roles, not key contents. Confirm that the server-side routine authorization is
   restricted to the intended repository and append-only behavior where used.
8. Which pinned `known_hosts` file is authoritative, and what independently
   verified Ed25519 host-key fingerprint should it contain? Never populate it
   from an unverified first connection.
9. Where are the encrypted recovery bundle, Borg passphrase, and exported Borg
   repokey? Which local Age/SSH identity can decrypt the bundle? If the original
   disk is gone, is there an independent recipient such as an offline recovery
   key or a surviving server host identity?
10. For replacement-host adoption, has the old writer been disabled and have all
    of its Boxup timers and running backup jobs stopped? Never allow two machines
    to write concurrently with the same host ID, routine key, and repository.
11. Which source paths are important, which mounts or Btrfs subvolumes cross
    filesystem boundaries, and which exclusions existed previously? Review every
    source against `one_file_system` and symlink targets instead of assuming that
    the fresh machine has the same layout.
12. Are Docker volumes, bind mounts, PostgreSQL instances, other databases, VMs,
    or systemd-managed services involved? Obtain the application-specific
    quiesce, dump, and restore requirements before claiming consistency.
13. Is the intended operation due-based for an intermittently powered desktop or
    calendar-based for an always-on server? Obtain the desired retention limits,
    rate limits, and maintenance policy before enabling timers.
14. For a restore, which exact archive and archive-relative paths are required,
    where should they be published, and is the configured staging directory on
    the same filesystem as that destination?

If the repository URI, passphrase, repository SSH key, verified host key, and a
working recovery identity cannot be established, stop. A public SSH key, a
GitHub checkout, or the SQLite index alone cannot recover encrypted backup data.

## Minimum Material For An Existing Repository

The minimum safe recovery set is:

- The versioned Boxup profile containing the exact repository and policy paths.
- The repository passphrase in a protected file.
- The routine SSH private key authorized for that repository.
- A pinned `known_hosts` entry verified independently of the recovery machine.
- Borg 1.4 and a Boxup build compatible with the profile version.

Also retain the exported encrypted Borg repokey, the maintenance key when
maintenance is required, the source-filesystem layout, workload recovery notes,
and a hash of the encrypted recovery bundle. A repokey repository normally reads
its encrypted key from the repository, but the independent export is required
for disaster recovery. Store recovery material separately from both the source
disk and the Borg repository.

## Install Boxup Without Creating State

1. Install only runtime dependencies first. On Arch/Omarchy these include Borg
   1.4, `python-pyfuse3`, FUSE 3, OpenSSH, Age, rsync, ACL tools, curl, SQLite,
   systemd, and polkit. On Debian/Ubuntu use the corresponding distribution
   packages, including `borgbackup`, `python3-pyfuse3`, `openssh-client`,
   `polkitd`, and `pkexec`.
2. Obtain Boxup from the trusted upstream repository or a previously verified
   native package. Building from source requires the Rust version declared in
   `Cargo.toml`; target systems do not need Rust after package installation.
3. Verify the Git commit or package checksum before installation. Never use a
   recovered binary whose provenance cannot be established.
4. Installing the package must not initialize a repository, create a profile,
   install credentials, or enable timers. Confirm this before continuing.

## Recover Credentials Safely

1. Work in a private mode-0700 directory on a trusted local filesystem. Do not
   decrypt onto a shared mount or cloud-synchronized directory, and do not place
   secret values in shell history.
2. Verify the encrypted recovery bundle hash before extracting it.
3. Decrypt each required file directly to a mode-0600 destination with Age. Do
   not print decrypted data to the terminal, command arguments, logs, or chat.
4. Confirm file types, ownership, modes, and non-symlink status. The setup helper
   intentionally rejects symlinked credential inputs and existing destinations.
5. Inspect the recovered profile for repository location, SSH port, Borg paths,
   source paths, excludes, staging paths, index path, and host ID. Secret values
   must remain external files referenced by path.
6. Verify the Storage Box host-key fingerprint out of band before any repository
   operation. A successful SSH connection is not proof that the host key is
   authentic.

Do not copy an SSH host private key off a surviving recovery server merely to
decrypt a bundle. Prefer decrypting through an audited console/session on that
server and transferring only the specific recovered credential files over a new,
verified secure channel.

## Reconnect An Existing Profile

For an existing repository, preserve the original `host.id`, repository URI,
encryption passphrase, routine key, and archive prefix. Update only paths that
must differ on the new operating system, and validate that the system-profile
filename remains exactly `/etc/boxup/HOST.toml`.

Provision the recovered profile without enabling anything:

```sh
pkexec /usr/lib/boxup/setup-profile \
  HOST RECOVERED.toml PASSPHRASE_FILE ROUTINE_SSH_KEY \
  VERIFIED_KNOWN_HOSTS MAINTENANCE_KEY_OR_DASH BROWSE_USER
```

When using `-`, the recovered profile must also omit
`repository.maintenance_ssh_key`; otherwise validation correctly rejects the
missing configured credential.

On a headless server, invoke the same fixed helper through an audited root
session. Do not weaken permissions to make the normal user read `/etc/boxup`.
The helper must report that it initialized no repository and enabled no timer.

For every existing repository, the next command is **never** `boxup init`.
Initialization is only for a separately verified, empty, new namespace. Running
it against a missing-looking path without first proving the intended URI is a
common way to create an unrelated empty repository and mistake it for data loss.

## Validate Read-Only Access First

1. Confirm the installed profile and browse descriptor refer to the intended
   host and repository.
2. Force a live snapshot listing rather than trusting a copied SQLite index:

```sh
boxup snapshots --live
```

3. Confirm that expected archive names, timestamps, and source roots are present.
4. Refresh the local browsing cache only after the live result is correct:

```sh
boxup --config /etc/boxup/HOST.toml index refresh
boxup status
```

5. List the exact parent path before restoring it:

```sh
boxup ls HOST-ARCHIVE home/user/Documents --live
```

If live snapshots differ from the expected host/archive set, stop and recheck
the repository URI, SSH endpoint, profile ID, and credentials. Do not initialize,
unlock, prune, compact, or delete anything as a diagnostic shortcut.

## Restore Selected Files Into A Fresh System

Always publish into an absent or empty dedicated destination first. Do not merge
an old home directory or all of `.config` over a newly installed desktop.

For a root-owned installed profile, run the normal non-overwrite restore through
an explicit administrative channel:

```sh
pkexec /usr/bin/boxup --config /etc/boxup/HOST.toml restore \
  HOST-ARCHIVE /home/user/Documents/project \
  --to /var/lib/boxup-recovery/project
```

Restore one logical group at a time, inspect it, and then copy only the selected
files into the fresh system. Preserve ownership, mode, ACLs, extended attributes,
timestamps, and symlink targets where they matter. Treat SSH/GPG keys and browser
profiles as secrets and restore them with their original restrictive modes.

Prefer restoring documents, source repositories, photos, application data,
configuration files selected per application, game saves, and world data.
Reinstall the operating system, bootloader, packages, runtimes, caches, game
installations, and other reproducible exclusions. Explain that paths excluded by
the profile or skipped by `one_file_system` cannot be recovered from that archive.

Never use `--overwrite --sudo` for normal migration. Emergency root overwrite is
non-transactional and remains subject to every restriction in `docs/RESTORE.md`.

## Adopt The Existing Repository As The Replacement Writer

Continue past read-only validation only when the user explicitly wants this new
machine to replace the old backup writer.

1. Prove the old machine's backup, maintenance, and check services are inactive.
2. Re-audit sources, mount boundaries, excludes, capacity, Docker/service staging,
   and database dumps on the new filesystem layout.
3. Perform one deliberate backup with timers still disabled.
4. Verify the new archive live, refresh the index, run the configured repository
   check, and restore representative files to an empty destination.
5. Verify the recovery bundle still decrypts through two independent recipients
   and that local/offsite encrypted bundle hashes match.
6. Enable only the timer matching the profile schedule after every gate passes.
7. Enable maintenance only when a separate maintenance credential is installed
   and its deletion authority is explicitly accepted.

Do not retire the old repository, credentials, recovery bundle, or source disk
until the new backup and actual restore rehearsal both succeed.

## Integrate Boxup As A New Deployment

For a genuinely new deployment, start from the closest example profile but audit
every value. Use a unique host ID, dedicated repository namespace, strong random
passphrase, dedicated routine SSH key, and preferably a separate maintenance key.
Do not share repositories or keys between hosts.

Before initialization:

1. Resolve and independently verify the Storage Box SSH endpoint and host key.
2. Create the smallest stable repository namespace and server-side restricted-key
   authorization without modifying unrelated authorized keys.
3. Audit all persistent data, filesystem boundaries, symlinks, exclusions,
   databases, containers, services, VMs, and available staging space.
4. Validate the final profile and provision it with `setup-profile` while timers
   remain disabled.
5. Prove the exact repository target is missing and unambiguous. Only then may an
   explicitly authorized new deployment run `boxup init`.
6. Export the encrypted Borg repokey immediately, create an encrypted versioned
   recovery bundle, encrypt it to at least two independent recipients, upload an
   offsite copy separate from the repository, and compare hashes.
7. Run the first complete backup, live/index comparison, repository check, and
   representative restore rehearsal before enabling timers or retiring any prior
   backup system.

## Completion Evidence

Do not report recovery or adoption as complete without recording the following
evidence without secrets:

- Boxup version and Git commit or package checksum.
- Profile ID and redacted repository endpoint/path.
- Verified Storage Box host-key fingerprint source.
- Live archive name, immutable Borg archive ID, and timestamp.
- Expected source roots and any known exclusions or filesystem-boundary gaps.
- Index/live comparison result.
- Check result and whether full data verification was requested.
- Restore destination and verified hashes, modes, ownership, and symlink targets.
- Recovery-bundle hash and successful decryption by independent recipients.
- Final timer/service state and confirmation that no second writer remains.
