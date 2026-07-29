_boxup_completion() {
    local cur command nested previous word index
    cur=${COMP_WORDS[COMP_CWORD]}
    previous=
    if (( COMP_CWORD > 0 )); then
        previous=${COMP_WORDS[COMP_CWORD-1]}
    fi

    case "$previous" in
        --config|--browse-config|--to)
            COMPREPLY=( $(compgen -f -- "$cur") )
            return
            ;;
    esac

    for ((index=1; index<COMP_CWORD; index++)); do
        word=${COMP_WORDS[index]}
        case "$word" in
            setup|automation|init|backup|status|snapshots|ls|search|restore|mount|umount|diff|prune|check|index|config|key|audit|tui|help)
                command=$word
                continue
                ;;
        esac
        if [[ -n $command && $word != -* ]]; then
            nested=$word
        fi
    done

    case "$command" in
        '')
            COMPREPLY=( $(compgen -W '--config --browse-config --help --version setup automation init backup status snapshots ls search restore mount umount diff prune check index config key audit tui help' -- "$cur") )
            ;;
        automation)
            COMPREPLY=( $(compgen -W 'enable disable status --json --help' -- "$cur") )
            ;;
        backup)
            COMPREPLY=( $(compgen -W 'run --help' -- "$cur") )
            ;;
        status)
            COMPREPLY=( $(compgen -W '--json --help' -- "$cur") )
            ;;
        snapshots)
            COMPREPLY=( $(compgen -W '--json --live --help' -- "$cur") )
            ;;
        ls)
            COMPREPLY=( $(compgen -W '--json --live --help' -- "$cur") )
            ;;
        search)
            COMPREPLY=( $(compgen -W '--all-snapshots --json --help' -- "$cur") )
            ;;
        restore)
            COMPREPLY=( $(compgen -W '--to --overwrite --sudo --help' -- "$cur") )
            ;;
        diff)
            COMPREPLY=( $(compgen -W '--json --help' -- "$cur") )
            ;;
        prune)
            COMPREPLY=( $(compgen -W '--dry-run --help' -- "$cur") )
            ;;
        check)
            COMPREPLY=( $(compgen -W '--verify-data --help' -- "$cur") )
            ;;
        index)
            COMPREPLY=( $(compgen -W 'refresh --help' -- "$cur") )
            ;;
        config)
            if [[ $nested == validate || $nested == browse-descriptor ]]; then
                COMPREPLY=( $(compgen -W '--system-profile --help' -- "$cur") )
            else
                COMPREPLY=( $(compgen -W 'validate browse-descriptor --help' -- "$cur") )
            fi
            ;;
        key)
            if [[ $nested == export ]]; then
                COMPREPLY=( $(compgen -W '--to --help' -- "$cur") )
            else
                COMPREPLY=( $(compgen -W 'export --help' -- "$cur") )
            fi
            ;;
        audit)
            if [[ $nested == docker ]]; then
                COMPREPLY=( $(compgen -W '--json --running --help' -- "$cur") )
            else
                COMPREPLY=( $(compgen -W 'docker --help' -- "$cur") )
            fi
            ;;
        *)
            COMPREPLY=( $(compgen -W '--help' -- "$cur") )
            ;;
    esac
}

complete -F _boxup_completion boxup
