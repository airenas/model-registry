use super::CmdResult;
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use clap_complete::{Shell, generate};
use std::io::Write;
use std::path::PathBuf;

pub const NAME: &str = "autocompletion";

/// `mr autocompletion` needs the whole CLI definition, so the registry handles it itself.
/// Completes `model:version` words from the bucket by calling the hidden `mr complete`.
const FISH_DYNAMIC: &str = r#"
function __mr_bucket_args
    set -l toks (commandline -opc)
    for i in (seq (count $toks))
        switch $toks[$i]
            case -b --bucket -g --group
                set -l n (math $i + 1)
                test $n -le (count $toks); and printf '%s\n' $toks[$i] $toks[$n]
            case '--bucket=*' '--group=*'
                printf '%s\n' $toks[$i]
        end
    end
end

function __mr_complete_ref
    mr complete (__mr_bucket_args) $argv[1] -- (commandline -ct) 2>/dev/null
end

# number of positional words already typed after the subcommand (skips -b/-g VALUE)
function __mr_npos
    set -l toks (commandline -opc)
    set -l n 0
    set -l skip 0
    set -l seen 0
    for t in $toks[2..-1]
        if test $skip -eq 1
            set skip 0
            continue
        end
        switch $t
            case -b --bucket -g --group
                set skip 1
            case '-*'
            case '*'
                if test $seen -eq 0
                    set seen 1
                else
                    set n (math $n + 1)
                end
        end
    end
    echo $n
end

function __mr_first_pos
    test (__mr_npos) -eq 0
end

function __mr_next_pos
    test (__mr_npos) -ge 1
end

complete -c mr -n '__fish_seen_subcommand_from fetch delete tag ls unlock; and __mr_first_pos' -f -a '(__mr_complete_ref)'
complete -c mr -n '__fish_seen_subcommand_from push; and __mr_first_pos' -f -a '(__mr_complete_ref --models)'
# the directory after model:version
complete -c mr -n '__fish_seen_subcommand_from push fetch; and __mr_next_pos' -F
"#;

/// Same idea for bash: wraps the generated `_mr` and adds `model:version` and path words.
/// Needs the bash-completion package (`:` is a word break, which it knows how to undo).
const BASH_DYNAMIC: &str = r#"
_mr_dyn() {
    declare -F _get_comp_words_by_ref >/dev/null || { _mr "$@"; return; }
    local cur words cword
    _get_comp_words_by_ref -n : cur words cword
    local sub="" npos=0 skip=0 i w prev
    local -a bucket=()
    for ((i = 1; i < cword; i++)); do
        w=${words[i]}
        if ((skip)); then
            skip=0
            bucket+=("$prev" "$w")
            continue
        fi
        case $w in
            -b | --bucket | -g | --group) skip=1 prev=$w ;;
            --bucket=* | --group=*) bucket+=("$w") ;;
            -*) ;;
            *) if [[ -z $sub ]]; then sub=$w; else ((npos++)); fi ;;
        esac
    done
    if [[ $cur == -* || -z $sub ]]; then
        _mr "$@"
        return
    fi
    local flag=""
    case "$sub:$npos" in
        fetch:0 | delete:0 | tag:0 | ls:0 | unlock:0) ;;
        push:0) flag=--models ;;
        push:1 | fetch:1)
            _filedir
            return
            ;;
        *)
            _mr "$@"
            return
            ;;
    esac
    local pre=${cur%"${cur##*:}"} line
    COMPREPLY=()
    while IFS= read -r line; do
        line=${line%%$'\t'*}
        [[ $line == "$cur"* ]] || continue
        COMPREPLY+=("${line#"$pre"}")
        [[ $line == *: ]] && compopt -o nospace 2>/dev/null
    done < <(mr complete "${bucket[@]}" $flag -- "$cur" 2>/dev/null)
}
complete -F _mr_dyn -o nosort -o bashdefault -o default mr
"#;

pub fn definition() -> ClapCommand {
    ClapCommand::new(NAME)
        .about("Install shell completion (fish or bash) for mr")
        .arg(
            Arg::new("print")
                .long("print")
                .action(ArgAction::SetTrue)
                .help("Print the completion script to stdout instead of installing it"),
        )
        .arg(
            Arg::new("shell")
                .long("shell")
                .value_parser(clap::value_parser!(Shell))
                .help("Shell to generate the script for (default: from $SHELL, else fish)"),
        )
}

pub fn run(cli: &mut ClapCommand, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
    let shell = match args.get_one::<Shell>("shell") {
        Some(s) => *s,
        None => detect_shell(std::env::var("SHELL").ok().as_deref()),
    };
    let mut script = Vec::new();
    generate(shell, cli, "mr", &mut script);
    match shell {
        Shell::Fish => script.extend_from_slice(FISH_DYNAMIC.as_bytes()),
        Shell::Bash => script.extend_from_slice(BASH_DYNAMIC.as_bytes()),
        _ => {}
    }
    if args.get_flag("print") {
        out.write_all(&script)?;
        return Ok(());
    }
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let xdg_data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (path, hint) = match shell {
        Shell::Fish => {
            let p = fish_path(xdg_config, home)?;
            let hint = format!("Restart fish or run `source {}`", p.display());
            (p, hint)
        }
        Shell::Bash => {
            let p = bash_path(xdg_data, home)?;
            let hint = format!(
                "Restart bash or run `source {}` (needs the bash-completion package)",
                p.display()
            );
            (p, hint)
        }
        _ => {
            return Err(format!(
                "installing is only supported for fish and bash; use `--shell {shell} --print`"
            )
            .into());
        }
    };
    install(&path, &script)?;
    writeln!(out, "Installed {shell} completion to {}", path.display())?;
    writeln!(out, "{hint}")?;
    Ok(())
}

fn detect_shell(shell_env: Option<&str>) -> Shell {
    match shell_env.and_then(|s| s.rsplit('/').next()) {
        Some("bash") => Shell::Bash,
        _ => Shell::Fish,
    }
}

/// Lazily loaded by bash-completion when `mr` is completed.
fn bash_path(xdg_data: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf, String> {
    let base = match (xdg_data.filter(|p| !p.as_os_str().is_empty()), home) {
        (Some(x), _) => x,
        (None, Some(h)) => h.join(".local/share"),
        (None, None) => return Err("cannot find the data dir: HOME is not set".into()),
    };
    Ok(base.join("bash-completion/completions/mr"))
}

fn fish_path(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf, String> {
    let base = match (xdg.filter(|p| !p.as_os_str().is_empty()), home) {
        (Some(x), _) => x,
        (None, Some(h)) => h.join(".config"),
        (None, None) => return Err("cannot find the config dir: HOME is not set".into()),
    };
    Ok(base.join("fish/completions/mr.fish"))
}

fn install(path: &std::path::Path, script: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_path_prefers_xdg() {
        assert_eq!(
            fish_path(Some("/x".into()), Some("/h".into())).unwrap(),
            PathBuf::from("/x/fish/completions/mr.fish")
        );
        assert_eq!(
            fish_path(None, Some("/h".into())).unwrap(),
            PathBuf::from("/h/.config/fish/completions/mr.fish")
        );
        assert!(fish_path(None, None).is_err());
    }

    #[test]
    fn bash_path_and_detection() {
        assert_eq!(
            bash_path(None, Some("/h".into())).unwrap(),
            PathBuf::from("/h/.local/share/bash-completion/completions/mr")
        );
        assert_eq!(
            bash_path(Some("/d".into()), None).unwrap(),
            PathBuf::from("/d/bash-completion/completions/mr")
        );
        assert_eq!(detect_shell(Some("/bin/bash")), Shell::Bash);
        assert_eq!(detect_shell(Some("/usr/bin/fish")), Shell::Fish);
        assert_eq!(detect_shell(None), Shell::Fish);
    }

    #[test]
    fn installs_script() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/mr.fish");
        install(&p, b"x").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
    }
}
