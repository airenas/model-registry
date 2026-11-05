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
            case -b --bucket
                set -l n (math $i + 1)
                test $n -le (count $toks); and printf '%s\n' -b $toks[$n]
            case '--bucket=*'
                printf '%s\n' $toks[$i]
        end
    end
end

function __mr_complete_ref
    mr complete (__mr_bucket_args) $argv[1] -- (commandline -ct) 2>/dev/null
end

# number of positional words already typed after the subcommand (skips -b VALUE)
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
            case -b --bucket
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

complete -c mr -n '__fish_seen_subcommand_from fetch delete tag list; and __mr_first_pos' -f -a '(__mr_complete_ref)'
complete -c mr -n '__fish_seen_subcommand_from push; and __mr_first_pos' -f -a '(__mr_complete_ref --models)'
# the directory after model:version
complete -c mr -n '__fish_seen_subcommand_from push fetch; and __mr_next_pos' -F
"#;

pub fn definition() -> ClapCommand {
    ClapCommand::new(NAME)
        .about("Install shell completion (fish) for mr, like `mr --autocompletion`")
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
                .default_value("fish")
                .help("Shell to generate the script for (installing is supported for fish only)"),
        )
}

pub fn run(cli: &mut ClapCommand, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
    let shell = *args.get_one::<Shell>("shell").ok_or("shell is required")?;
    let mut script = Vec::new();
    generate(shell, cli, "mr", &mut script);
    if shell == Shell::Fish {
        script.extend_from_slice(FISH_DYNAMIC.as_bytes());
    }
    if args.get_flag("print") {
        out.write_all(&script)?;
        return Ok(());
    }
    if shell != Shell::Fish {
        return Err(format!(
            "installing is only supported for fish; use `--shell {shell} --print`"
        )
        .into());
    }
    let path = fish_path(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    install(&path, &script)?;
    writeln!(out, "Installed fish completion to {}", path.display())?;
    writeln!(out, "Restart fish or run `source {}`", path.display())?;
    Ok(())
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
    fn installs_script() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/mr.fish");
        install(&p, b"x").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
    }
}
