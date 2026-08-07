use clap::ValueEnum;

/// Shells for which fog can generate a completion script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

/// Shell expression that expands to the branches of every worktree in the
/// current repository.
const WORKTREE_BRANCHES: &str =
    r#"$(git worktree list --porcelain 2>/dev/null | sed -n 's/^branch refs\/heads\///p')"#;

/// Generates a self-contained completion script for the given shell.
///
/// `--branch` values are completed from the branches of every worktree in the
/// current repository.
pub fn generate(shell: CompletionShell) -> String {
    match shell {
        CompletionShell::Bash => bash_script(),
        CompletionShell::Zsh => zsh_script(),
        CompletionShell::Fish => fish_script(),
    }
}

fn bash_script() -> String {
    let branches = WORKTREE_BRANCHES;
    format!(
        r##"# bash completion for fog
_fog() {{
    local cur prev opts
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"

    if [[ "$prev" == "--branch" ]]; then
        COMPREPLY=( $(compgen -W "{branches}" -- "$cur") )
        return 0
    fi
    if [[ "$cur" == --branch=* ]]; then
        COMPREPLY=( $(compgen -W "{branches}" -- "${{cur#--branch=}}") )
        return 0
    fi

    opts="--config --save-logs --branch --completions -h --help -V --version"
    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
    else
        COMPREPLY=( $(compgen -W "ls kill" -- "$cur") )
    fi
    return 0
}}
complete -F _fog fog
"##,
    )
}

fn zsh_script() -> String {
    let branches = WORKTREE_BRANCHES;
    format!(
        r##"#compdef fog
# zsh completion for fog
_fog() {{
    local context state line
    typeset -A opt_args
    _arguments \
        '(-c --config)'{{-c,--config}}'[Path to config file]:file:_files' \
        '--save-logs[Save service output]' \
        '--branch=[Run in the worktree of a branch]:branch:->branches' \
        '--completions=[Generate a completion script]:shell:(bash zsh fish)' \
        '(-h --help)'{{-h,--help}}'[Print help]' \
        '(-V --version)'{{-V,--version}}'[Print version]' \
        '1:command:(ls kill)' \
        '*:pid:'
    case $state in
        branches)
            local -a list
            list=(${{(f)"{branches}"}})
            _describe -t branch 'branch' list
            ;;
    esac
}}
compdef _fog fog
"##,
    )
}

fn fish_script() -> String {
    let branches = WORKTREE_BRANCHES;
    format!(
        r##"# fish completion for fog
complete -c fog -s c -l config -d 'Path to config file' -r
complete -c fog -l save-logs -d 'Save service output to temp/ on exit'
complete -c fog -l branch -d 'Run in the worktree of a branch' -a '({branches})'
complete -c fog -l completions -d 'Generate a completion script' -a 'bash zsh fish'
complete -c fog -s h -l help -d 'Print help'
complete -c fog -s V -l version -d 'Print version'
complete -c fog -f -a 'ls kill'
"##,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_bash_lists_worktree_branches() {
        let out = generate(CompletionShell::Bash);
        assert!(out.contains("git worktree list"));
        assert!(out.contains("--branch"));
        assert!(out.contains("complete -F _fog fog"));
    }

    #[test]
    fn test_generate_zsh_lists_worktree_branches() {
        let out = generate(CompletionShell::Zsh);
        assert!(out.contains("git worktree list"));
        assert!(out.contains("--branch"));
        assert!(out.contains("compdef _fog fog"));
    }

    #[test]
    fn test_generate_fish_lists_worktree_branches() {
        let out = generate(CompletionShell::Fish);
        assert!(out.contains("git worktree list"));
        assert!(out.contains("-l branch"));
        assert!(out.contains("complete -c fog"));
    }
}
