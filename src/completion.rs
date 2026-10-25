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
    local cur prev opts i
    local -i index_pos=0
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"

    if [[ "$prev" == "--branch" ]]; then
        COMPREPLY=( $(compgen -W "{branches}" -- "$cur") )
        return 0
    fi
    if [[ "$cur" == --branch=* ]]; then
        COMPREPLY=( $(compgen -W "{branches}" -- "${{cur#--branch=}}" ) )
        return 0
    fi
    if [[ "$prev" == "--config" || "$prev" == "-c" ]]; then
        COMPREPLY=( $(compgen -f -- "$cur") )
        return 0
    fi
    if [[ "$prev" == "--completions" ]]; then
        COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") )
        return 0
    fi
    if [[ "$prev" == "--service" || "$prev" == "-s" ]]; then
        return 0
    fi
    if [[ "$prev" == "--head" || "$prev" == "--tail" || "$prev" == "--port" ]]; then
        return 0
    fi
    if [[ "$cur" == --port=* ]]; then
        return 0
    fi

    # Locate `index` after any global flags, so `fog -v index serve` completes
    # like `fog index serve`.
    for (( i = 1; i < COMP_CWORD; i++ )); do
        if [[ "${{COMP_WORDS[i]}}" == "index" ]]; then
            index_pos=$i
            break
        fi
    done
    if (( index_pos > 0 )); then
        if (( COMP_CWORD == index_pos + 1 )); then
            COMPREPLY=( $(compgen -W "serve kill restart" -- "$cur") )
        elif [[ "${{COMP_WORDS[index_pos+1]}}" == "serve" ]]; then
            COMPREPLY=( $(compgen -W "--foreground --port" -- "$cur") )
        fi
        return 0
    fi

    opts="--config --save-logs --branch --detach --service --head --tail --all --force --no-share --port --dry-run --completions --verbose -c -d -s -v -h --help -V --version"
    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
    else
        COMPREPLY=( $(compgen -W "ls kill restart logs prune attach index" -- "$cur") )
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
    local -i index_pos=0 i
    for (( i = 2; i <= CURRENT; i++ )); do
        if [[ $words[i] == index ]]; then
            index_pos=$i
            break
        fi
    done
    if (( index_pos )); then
        if (( CURRENT == index_pos + 1 )); then
            _values 'index command' serve kill restart
        elif [[ $words[index_pos+1] == serve ]]; then
            _arguments '--foreground[Run in the foreground]' '--port=[Port for the index server]:port:'
        fi
        return 0
    fi

    local context state line
    typeset -A opt_args
    _arguments \
        '(-c --config)'{{-c,--config}}'[Path to config file]:file:_files' \
        '--save-logs[Save service output]' \
        '--branch=[Run in the worktree of a branch]:branch:->branches' \
        '(-d --detach)'{{-d,--detach}}'[Run in the background without the TUI]' \
        '(-s --service)'{{-s,--service}}'[Show logs for one service]:service:' \
        '--head=[First N lines, or all but last N with -N (fog logs)]:n:' \
        '--tail=[Last N lines, or from +N to the end (fog logs)]:n:' \
        '--all[Apply to every matching instance (kill/restart)]' \
        '--force[Escalate to SIGTERM/SIGKILL if it does not stop (kill/restart)]' \
        '--no-share[Ignore shared services]' \
        '--port=[Override a top-level ports entry for this run]:port:' \
        '--dry-run[Report what would be removed without deleting anything (fog prune)]' \
        '--completions=[Generate a completion script]:shell:(bash zsh fish)' \
        '(-v --verbose)'{{-v,--verbose}}'[Print verbose setup output]' \
        '(-h --help)'{{-h,--help}}'[Print help]' \
        '(-V --version)'{{-V,--version}}'[Print version]' \
        '1:command:(ls kill restart logs prune attach index)' \
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
complete -c fog -s d -l detach -d 'Run in the background without the TUI'
complete -c fog -s s -l service -d 'Show logs for one service (fog logs)'
complete -c fog -l head -d 'First N lines, or all but last N with -N (fog logs)' -r
complete -c fog -l tail -d 'Last N lines, or from +N to the end (fog logs)' -r
complete -c fog -l all -d 'Apply to every matching instance (kill/restart)'
complete -c fog -l force -d 'Escalate to SIGTERM/SIGKILL if it does not stop (kill/restart)'
complete -c fog -l no-share -d 'Ignore shared services'
complete -c fog -l port -d 'Override a top-level ports entry for this run (NAME=PORT)' -r
complete -c fog -l dry-run -d 'Report what would be removed without deleting anything (fog prune)'
complete -c fog -l completions -d 'Generate a completion script' -a 'bash zsh fish'
complete -c fog -s v -l verbose -d 'Print verbose setup output'
complete -c fog -s h -l help -d 'Print help'
complete -c fog -s V -l version -d 'Print version'
complete -c fog -f -a 'ls kill restart logs prune attach index'
complete -c fog -f -n 'test (count (commandline -opc)) -ge 2; and test (commandline -opc)[2] = index' -a 'serve kill restart' -d 'index command'
complete -c fog -n 'test (count (commandline -opc)) -ge 3; and test (commandline -opc)[2] = index; and test (commandline -opc)[3] = serve' -l foreground -d 'Run the index server in the foreground'
complete -c fog -n 'test (count (commandline -opc)) -ge 3; and test (commandline -opc)[2] = index; and test (commandline -opc)[3] = serve' -l port -r -d 'Index server port'
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
        assert!(out.contains("--port"));
        assert!(out.contains("--head"));
        assert!(out.contains("--tail"));
        assert!(out.contains("--no-share"));
        assert!(out.contains("logs prune attach"));
        assert!(out.contains("--dry-run"));
        assert!(out.contains("serve kill restart"));
        assert!(out.contains("--foreground"));
        assert!(out.contains("complete -F _fog fog"));
    }

    #[test]
    fn test_generate_zsh_lists_worktree_branches() {
        let out = generate(CompletionShell::Zsh);
        assert!(out.contains("git worktree list"));
        assert!(out.contains("--branch"));
        assert!(out.contains("--port"));
        assert!(out.contains("--head"));
        assert!(out.contains("--tail"));
        assert!(out.contains("--no-share"));
        assert!(out.contains("logs prune attach"));
        assert!(out.contains("--dry-run"));
        assert!(out.contains("serve kill restart"));
        assert!(out.contains("--foreground"));
        assert!(out.contains("compdef _fog fog"));
    }

    #[test]
    fn test_generate_fish_lists_worktree_branches() {
        let out = generate(CompletionShell::Fish);
        assert!(out.contains("git worktree list"));
        assert!(out.contains("-l branch"));
        assert!(out.contains("-l port"));
        assert!(out.contains("-l head"));
        assert!(out.contains("-l tail"));
        assert!(out.contains("-l no-share"));
        assert!(out.contains("logs prune attach"));
        assert!(out.contains("-l dry-run"));
        assert!(out.contains("serve kill restart"));
        assert!(out.contains("-l foreground"));
        assert!(out.contains("complete -c fog"));
    }

    /// Runs the generated bash script in a real shell, so a position-sensitive
    /// bug - "index is only recognised as the first word" - cannot hide behind
    /// a substring assertion.
    #[cfg(unix)]
    fn bash_completion(words: &[&str]) -> Option<Vec<String>> {
        let script =
            std::env::temp_dir().join(format!("fog-completion-{}.bash", std::process::id()));
        std::fs::write(&script, generate(CompletionShell::Bash)).ok()?;
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(
                r#"source "$1" || exit 1; shift
COMP_WORDS=("$@"); COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 )); _fog
for candidate in "${COMPREPLY[@]}"; do printf 'candidate:%s\n' "$candidate"; done"#,
            )
            .arg("fog-completion-test")
            .arg(script.to_string_lossy().replace('\\', "/"))
            .args(words)
            .output()
            .ok();
        let _ = std::fs::remove_file(&script);
        let output = output?;
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("candidate:").map(str::to_string))
                .collect()
        })
    }

    #[test]
    #[cfg(unix)]
    fn test_bash_completion_probes_index_and_flag_values() {
        if bash_completion(&["fog", ""]).is_none() {
            return;
        }
        let candidate = |words: &[&str]| -> Vec<String> {
            bash_completion(words).expect("bash must run the generated script")
        };
        assert_eq!(
            candidate(&["fog", "index", ""]),
            ["serve", "kill", "restart"]
        );
        assert_eq!(
            candidate(&["fog", "index", "serve", "-"]),
            ["--foreground", "--port"]
        );
        assert!(candidate(&["fog", "index", "serve", "--port", ""]).is_empty());
        assert!(candidate(&["fog", "index", "serve", "--port=18"]).is_empty());
        assert_eq!(
            candidate(&["fog", "-v", "index", "serve", "-"]),
            ["--foreground", "--port"]
        );
        assert!(candidate(&["fog", "logs", "--tail", ""]).is_empty());
        assert!(
            candidate(&["fog", "-"])
                .iter()
                .any(|flag| flag == "--no-share")
        );
        assert!(
            candidate(&["fog", "prune", "-"])
                .iter()
                .any(|flag| flag == "--dry-run")
        );
        assert!(
            candidate(&["fog", ""])
                .iter()
                .any(|command| command == "prune")
        );
    }
}
