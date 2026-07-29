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

## Everyday Use

Run `boxup` without arguments. It opens the English terminal application with a
small numbered menu:

```text
1 Back up now   2 Restore files   3 Browse backups
4 Settings/details   5 Toggle automation   Q Quit
```

The normal dashboard uses plain states such as `Protected`, `Backing up`,
`Needs attention`, `Backup failed`, and `Automatic backups off`. Repository,
index, and Borg details stay behind the settings/details view. While a foreground
or scheduled backup is active, the dashboard shows its current phase, elapsed
time, processed files and bytes, newly stored data, and last progress update.

When no profile is installed, `boxup` starts a guided wizard for reconnecting an
existing repository from its exact recovered profile. It asks only for profile
and credential file paths, never secret contents. The wizard preserves the old
host ID, repository, sources, exclusions, and schedule; it never initializes a
repository and leaves automation off until live read-only validation, a safe-copy
restore, and a deliberate backup have succeeded.

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

For the guided existing-repository flow, run `boxup setup`. The manual expert
flow remains available below.

Choose a short host ID such as `desktop`. The filename and `host.id` must match.

```sh
cp examples/desktop.toml desktop.toml
```

After editing and reviewing the profile, install it with the fixed setup helper:

```sh
pkexec /usr/lib/boxup/setup-profile \
  desktop desktop.toml desktop.passphrase desktop_ed25519 \
  known_hosts desktop_maintenance_ed25519 YOUR_USER reconnect
```

When automatic prune is not wanted, remove `maintenance_ssh_key` from the TOML
profile and use `-` instead of the maintenance-key argument. The helper creates
root-only credentials and a secret-free browse descriptor for the selected user.
It still does not initialize anything or enable timers.

The setup also installs one exact sudoers rule for that user and profile. It
permits only this passwordless command:

```text
/usr/lib/boxup/boxup-root --config /etc/boxup/HOST.toml backup --progress-json
```

It does not grant passwordless access to `boxup`, restore, initialization,
maintenance, checks, key export, or arbitrary root commands. Existing profiles
created before this feature need one explicit administrative migration:

```sh
sudo /usr/lib/boxup/setup-backup-sudo HOST BROWSE_USER
```

That migration can require authentication once. Afterwards, `boxup backup` and
the TUI `B` action use `sudo -n` and never display a password prompt.

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
For a separately verified new deployment, install its profile with `new` instead
of `reconnect`; only that explicit setup mode permits the later `boxup init`.

## First Backup

Keep timers disabled for the first run:

```sh
boxup backup
boxup status
boxup snapshots --live
```

Manual `boxup backup` runs in the foreground and reports its current phase,
elapsed time, processed files and bytes, throughput, and newly deduplicated data.
After three successful runs it also shows a clearly approximate progress bar and
ETA based on the median duration of recent successful backups. Borg does not
provide a total input size while creating an archive, so Boxup does not claim an
exact percentage and does not pre-scan every source. Current source paths are
never included in progress output.

For an installed system profile, the foreground command uses the fixed
privileged helper. Scheduled backups continue to use the sandboxed systemd
services. A manual foreground run is terminal-coupled and does not inherit the
service unit's resource limits or sandbox. Pressing Ctrl-C requests cancellation;
Boxup defers it while configured
containers or services are quiesced and records the interrupted run as failed
after application data is safely resumed.

After the independent browsing index has refreshed, confirm its freshness and
perform a real restore test:

```sh
boxup status
boxup ls desktop-ARCHIVE home/YOUR_USER/Documents --live
```

Backups do not wait for the file browsing index to refresh. The independent
`boxup-index@HOST.timer` updates that cache in the background when enabled. You
can also run `boxup index refresh` explicitly. When a single installed browse
descriptor is discovered, it automatically delegates to its matching system
profile.

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

The terminal browser asks whether to restore a safe copy or replace the original
path. Safe copy is the default and publishes into a new root-owned directory
under `/var/lib/boxup-recovery/HOST`. Original replacement remains an advanced
choice: select entries with Space, press `R`, choose replacement, review the
displayed `/...` targets, and type `RESTORE`. For example, the
archive path `home/alice/.config/hypr` exactly replaces
`/home/alice/.config/hypr`; files present only in the current directory are
removed. The TUI displays live validation, Borg extraction, verification, and
publication progress.

Original-path restore is available only with a system profile under
`/etc/boxup` and the fixed privileged helper. Staging and every selected target
must resolve to one filesystem; Boxup creates protected fallback staging on that
filesystem when needed. Prefer one logical path per operation and use a normal
restore when you need to inspect data before replacement.

Root overwrite exists only for disaster recovery and is intentionally difficult.
See [Restore And Recovery](docs/RESTORE.md) for mounts, Docker data, metadata,
rehearsals, and emergency restore behavior.

## Enable Scheduling

After a successful first backup and restore rehearsal, enable daily backups,
six-hour background indexing, and per-backup desktop notifications together:

```sh
boxup automation enable
boxup automation status
```

Use `boxup automation disable` to stop unattended repository writes and
background index reads.
The desktop notification watcher starts with the user's next login. Desktop
profiles use a due-based timer, suitable for machines that are not always online;
with the friendly default, one backup becomes due every 24 hours and is started
after the computer is next available. Server profiles use a calendar timer.

The equivalent expert-level units are:

```sh
# Desktop profile
sudo systemctl enable --now boxup-backup-desktop@desktop.timer

# Calendar/server profile
sudo systemctl enable --now boxup-backup-server@HOST.timer

# Optional after successful recovery verification
sudo systemctl enable --now boxup-maintenance@HOST.timer
sudo systemctl enable --now boxup-check@HOST.timer

# Optional independent background index refresh
sudo systemctl enable --now boxup-index@HOST.timer
```

Enable only the backup timer matching the profile schedule. Maintenance requires
a configured key with delete access and therefore weakens append-only protection.
The index timer starts after boot and refreshes every six hours with a randomized
delay. Packages install all timers disabled. `boxup automation enable` selects
exactly the backup timer matching the profile and enables the index timer; it
never enables maintenance or repository checks.

## Common Commands

```text
boxup
boxup setup
boxup automation enable|disable|status
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

`boxup status` reports the last fully successful Boxup workflow, its archive,
duration and final Borg statistics, the next due time, the latest attempt,
active or stale jobs, recent job history, historical duration estimate, and
local index freshness. This command reads local cached state and does not contact
the repository; use `boxup snapshots --live` for live validation.

Installed packages provide Bash, Zsh, and Fish completions. Start a new shell
after package installation, then commands and nested commands such as
`boxup index refresh`, `boxup config validate`, and `boxup key export` complete
with Tab. Bash completion is installed as a runtime dependency.

The SQLite index is an untrusted browsing cache. The optional index timer keeps it
synchronized independently from backups. An index refresh performs read-only
repository access aside from writes to local cache and index state. Restore,
retention, initialization, and repository checks use live Borg data rather than
trusting SQLite.

The terminal app opens on a simple local-status dashboard. Numbered actions start
a backup, open the restore flow, browse snapshots, reveal technical details, or
toggle automation. The dashboard remains available when the index is stale or
incomplete, while browsing stays denied. A known Borg warning that files changed
while being read and configured folders that are currently absent are recorded
as a successful backup with notes; unknown, permission, skipped-file, and
repository warnings remain failures. Failed
foreground backups display a bounded sanitized message in a wrapped dialog.
The browser supports Yazi-style navigation: `j`/`k` move down/up, `l` opens a
directory or enters the file pane, and `h` goes to the parent directory or back
to the snapshot pane. Arrow keys, Enter, Backspace, Home, and End remain
available. `/` filters only the directly visible entries in the currently open
directory; it does not search the complete snapshot.

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
