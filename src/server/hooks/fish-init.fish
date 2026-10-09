# nt shell integration for fish (written by nt; do not edit). Runs after config.fish.
# Newer fish emits OSC 133 itself; the server ignores the duplicates.
function __nt_esc
    set -l o ''
    for c in (string split '' -- (string sub -l 1000 -- "$argv[1]" | string collect))
        if string match -rq -- '^[ -:<-\[\]-~]$' $c
            set o $o$c
        else
            for h in (printf '%s' "$c" | od -An -v -tx1 | string trim | string split ' ')
                test -n "$h"; and set o $o\\x$h
            end
        end
    end
    printf '%s' $o
end

function __nt_osc7
    set -l p (string escape --style=url -- $PWD)
    printf '\e]7;file://%s%s\a' (hostname) $p
end

function __nt_postexec --on-event fish_postexec
    set -l ec $status
    printf '\e]133;D;%s\a' $ec
end

function __nt_prompt --on-event fish_prompt
    __nt_osc7
    printf '\e]133;A\a'
end

function __nt_preexec --on-event fish_preexec
    set -l e (__nt_esc "$argv[1]")
    printf '\e]133;E;%s\a\e]133;C\a' $e
end
