# Boxup

Boxup is a safe Linux frontend for Borg 1.4 backups. It provides:

- encrypted, compressed Borg backups;
- a searchable local file index and terminal browser;
- staged restores that do not overwrite existing files by default;
- systemd timers for desktops and servers;
- optional Docker, PostgreSQL, and systemd-service quiescing;
- native packages for Arch Linux and Debian/Ubuntu.

Boxup does not provide cloud storage. You bring an existing Borg-compatible SSH
server, such as a Hetzner Storage Box, or use a local Borg repository.

## Important Rule

An existing repository must never be initialized again.

- Use `boxup init` only for a verified new and empty repository path.
- To reconnect an existing backup, install its original profile, passphrase, SSH
  key, and pinned `known_hosts` entry, then start with `boxup snapshots --live`.
- Keep one repository and one set of SSH keys per host.

See [Restore And Recovery](docs/RESTORE.md) before reinstalling a machine or
moving an existing profile.

## Install

Boxup requires Rust 1.85 or newer when building from source. Installed packages
do not require Rust.

```sh
git clone https://github.com/Xclipsen/boxup.git
cd boxup
sh scripts/bootstrap.sh
```

The bootstrap script builds and installs the native package for Arch or
Debian/Ubuntu. It does not create a profile, initialize a repository, install
credentials, or enable timers.

Manual build:

```sh
cargo build --locked --release
cargo test --locked
```

## What You Need

Prepare these files before configuring a host:

- a profile based on `examples/desktop.toml` or
  `examples/ubuntu-docker-vps.toml`;
- a Borg repository passphrase file;
- a private SSH key authorized for that repository;
- a pinned `known_hosts` file with a separately verified server fingerprint;
- optionally, a separate maintenance SSH key with delete access.

The profile contains paths to secrets, never the secret values themselves.
Review its repository URI, host ID, sources, exclusions, filesystem boundaries,
retention, restore limits, and schedule before installation.

For a Storage Box, confirm all of the following instead of assuming that a local
SSH alias will exist elsewhere:

- SSH hostname, username, and port;
- repository path relative to the SSH account;
- remote Borg executable, commonly `borg-1.4`;
- routine and maintenance key roles;
- independently verified SSH host-key fingerprint.

A routine key can be restricted to append-only access in the server's
`authorized_keys` file. Adapt the account home and repository path to your
provider:

```text
restrict,command="borg-1.4 serve --append-only --restrict-to-repository /home/BACKUP_USER/boxup/desktop/repository" ssh-ed25519 ROUTINE_PUBLIC_KEY
```

## Create A Profile

Choose a short host ID such as `desktop`. The filename and `host.id` must match.

```sh
cp examples/desktop.toml desktop.toml
```

After editing and reviewing the profile, install it with the fixed setup helper:

```sh
pkexec /usr/lib/boxup/setup-profile \
  desktop desktop.toml desktop.passphrase desktop_ed25519 \
  known_hosts desktop_maintenance_ed25519 YOUR_USER
```

When automatic prune is not wanted, remove `maintenance_ssh_key` from the TOML
profile and use `-` instead of the maintenance-key argument. The helper creates
root-only credentials and a secret-free browse descriptor for the selected user.
It still does not initialize anything or enable timers.

## New Repository

Only continue here when the configured repository path is known to be new and
empty:

```sh
boxup --config /etc/boxup/desktop.toml init
boxup --config /etc/boxup/desktop.toml key export
```

Immediately store the exported repokey, passphrase, SSH key, pinned host key, and
exact profile outside both the source machine and the repository. Test that the
recovery copy can be decrypted before relying on the backup.

Skip this entire section when reconnecting an existing repository.

## First Backup

Keep timers disabled for the first run:

```sh
boxup backup
boxup status
boxup snapshots --live
```

Then refresh the local browsing index and perform a real restore test:

```sh
boxup --config /etc/boxup/desktop.toml index refresh
boxup ls desktop-ARCHIVE home/YOUR_USER/Documents --live
```

Do not retire an older backup system until a complete Boxup backup, repository
check, and representative restore have all succeeded.

## Restore Files

Always restore into a new or empty directory first:

```sh
pkexec /usr/bin/boxup --config /etc/boxup/desktop.toml restore \
  desktop-ARCHIVE /home/YOUR_USER/Documents/project \
  --to /var/lib/boxup-recovery/project
```

Inspect the restored files before copying selected data into the live system.
Avoid restoring an entire old `.config` over a fresh desktop installation.

Root overwrite exists only for disaster recovery and is intentionally difficult.
See [Restore And Recovery](docs/RESTORE.md) for mounts, Docker data, metadata,
rehearsals, and emergency restore behavior.

## Enable Scheduling

Desktop profiles use a due-based timer, suitable for machines that are not always
online. Server profiles use a calendar timer.

```sh
# Desktop profile
sudo systemctl enable --now boxup-backup-desktop@desktop.timer

# Calendar/server profile
sudo systemctl enable --now boxup-backup-server@HOST.timer

# Optional after successful recovery verification
sudo systemctl enable --now boxup-maintenance@HOST.timer
sudo systemctl enable --now boxup-check@HOST.timer
```

Enable only the backup timer matching the profile schedule. Maintenance requires
a configured key with delete access and therefore weakens append-only protection.

## Common Commands

```text
boxup backup
boxup status [--json]
boxup snapshots [--json] [--live]
boxup ls SNAPSHOT [PATH] [--json] [--live]
boxup search QUERY [--all-snapshots]
boxup restore SNAPSHOT PATH... --to DESTINATION
boxup mount SNAPSHOT TARGET
boxup umount TARGET
boxup diff SNAPSHOT_A SNAPSHOT_B [PATH]
boxup check [--verify-data]
boxup prune [--dry-run]
boxup index refresh
boxup tui
```

The SQLite index is only a browsing cache. Restore, retention, initialization,
and repository checks use live Borg data.

## Docker Hosts

Docker support is disabled by default. When enabled, Boxup can stage selected
bind mounts and volumes, create PostgreSQL logical dumps, and stop configured
containers or services for a final `rsync -aHAXS` copy.

Audit every persistent mount and database before enabling it. Boxup excludes a
mount from the ordinary backup only after that exact source was staged
successfully. Other databases need their own tested consistency procedure.

Start with `examples/ubuntu-docker-vps.toml` and verify the result with:

```sh
pkexec /usr/bin/boxup --config /etc/boxup/HOST.toml audit docker --json
```

## Safety Model

- Secrets stay in root-only files and are not passed in command arguments.
- Borg, SSH, rsync, Docker, curl, and systemctl run without a shell.
- Restore paths are validated against live repository data and protected paths.
- Normal restore refuses non-empty destinations and symlink traversal.
- Repository initialization refuses existing or ambiguous targets.
- Routine backup keys can be append-only; maintenance keys are separate.
- Packages install immutable files only and never enable services automatically.

Detailed operational constraints are in [AGENTS.md](AGENTS.md), recovery behavior
is documented in [docs/RESTORE.md](docs/RESTORE.md), and security reports follow
[SECURITY.md](SECURITY.md).

Project: `https://github.com/Xclipsen/boxup`.
