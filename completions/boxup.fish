complete -c boxup -f
complete -c boxup -l config -r -F -d 'Configuration file'
complete -c boxup -l browse-config -r -F -d 'Browse descriptor'
complete -c boxup -s h -l help -d 'Show help'
complete -c boxup -s V -l version -d 'Show version'

complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a setup -d 'Open the guided setup'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a automation -d 'Manage automatic backups and indexing'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a init -d 'Initialize a verified new repository'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a backup -d 'Create a backup archive'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a status -d 'Show local backup status'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a snapshots -d 'List snapshots'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a ls -d 'List archive contents'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a search -d 'Search the local index'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a restore -d 'Restore selected paths'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a mount -d 'Mount a snapshot'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a umount -d 'Unmount a snapshot'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a diff -d 'Compare snapshots'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a prune -d 'Apply retention policy'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a check -d 'Check repository integrity'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a index -d 'Manage the local index'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a config -d 'Validate or describe configuration'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a key -d 'Manage repository key export'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a audit -d 'Audit configured workloads'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui' -a tui -d 'Open the terminal dashboard'
complete -c boxup -n 'not __fish_seen_subcommand_from setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui help' -a help -d 'Show command help'

complete -c boxup -n '__fish_seen_subcommand_from backup' -a run
complete -c boxup -n '__fish_seen_subcommand_from automation' -a 'enable disable status'
complete -c boxup -n '__fish_seen_subcommand_from automation status' -l json
complete -c boxup -n '__fish_seen_subcommand_from status' -l json
complete -c boxup -n '__fish_seen_subcommand_from snapshots ls' -l json
complete -c boxup -n '__fish_seen_subcommand_from snapshots ls' -l live
complete -c boxup -n '__fish_seen_subcommand_from search' -l all-snapshots
complete -c boxup -n '__fish_seen_subcommand_from search' -l json
complete -c boxup -n '__fish_seen_subcommand_from restore' -l to -r -F
complete -c boxup -n '__fish_seen_subcommand_from restore' -l overwrite
complete -c boxup -n '__fish_seen_subcommand_from restore' -l sudo
complete -c boxup -n '__fish_seen_subcommand_from diff' -l json
complete -c boxup -n '__fish_seen_subcommand_from prune' -l dry-run
complete -c boxup -n '__fish_seen_subcommand_from check' -l verify-data
complete -c boxup -n '__fish_seen_subcommand_from index' -a refresh
complete -c boxup -n '__fish_seen_subcommand_from config' -a 'validate browse-descriptor'
complete -c boxup -n '__fish_seen_subcommand_from validate browse-descriptor' -l system-profile -r -F
complete -c boxup -n '__fish_seen_subcommand_from key' -a export
complete -c boxup -n '__fish_seen_subcommand_from export' -l to -r -F
complete -c boxup -n '__fish_seen_subcommand_from audit' -a docker
complete -c boxup -n '__fish_seen_subcommand_from docker' -l json
complete -c boxup -n '__fish_seen_subcommand_from docker' -l running
