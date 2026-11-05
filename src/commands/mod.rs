pub mod complete;
pub mod completion;
pub mod delete;
pub mod fetch;
pub mod info;
pub mod init;
pub mod list;
pub mod push;
pub mod tag;
pub mod version;

use crate::storage::{Storage, StorageResult, s3::S3Storage};
use clap::{Arg, ArgMatches, Command as ClapCommand};
use std::error::Error;
use std::io::Write;

pub type CmdResult = Result<(), Box<dyn Error>>;

/// Creates a storage for a bucket; replaceable in tests.
pub type StorageFactory = Box<dyn Fn(&str) -> StorageResult<Box<dyn Storage>>>;

/// `-j/--jobs`: number of files transferred in parallel.
pub fn jobs_arg() -> clap::Arg {
    clap::Arg::new("jobs")
        .short('j')
        .long("jobs")
        .help("Number of files to transfer in parallel")
        .value_parser(clap::value_parser!(usize))
        .default_value("4")
}

pub fn s3_factory() -> StorageFactory {
    Box::new(|bucket| Ok(Box::new(S3Storage::new(bucket)?) as Box<dyn Storage>))
}

/// A CLI subcommand (e.g. `version`, later `push`, `checkout`).
pub trait Command {
    /// Clap definition: name, about, and arguments of the subcommand.
    fn definition(&self) -> ClapCommand;
    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult;
}

/// Builds the clap CLI from registered commands and dispatches to them.
#[derive(Default)]
pub struct Registry {
    commands: Vec<Box<dyn Command>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with all built-in commands.
    pub fn default_commands() -> Self {
        let mut r = Self::new();
        r.register(Box::new(tag::Tag::new()));
        r.register(Box::new(complete::Complete::new()));
        r.register(Box::new(delete::Delete::new()));
        r.register(Box::new(fetch::Fetch::new()));
        r.register(Box::new(info::Info::new()));
        r.register(Box::new(init::Init::new()));
        r.register(Box::new(list::List::new()));
        r.register(Box::new(push::Push::new()));
        r.register(Box::new(version::Version::new()));
        r
    }

    pub fn register(&mut self, cmd: Box<dyn Command>) {
        self.commands.push(cmd);
    }

    pub fn cli(&self) -> ClapCommand {
        let mut cli = ClapCommand::new("mr")
            .about("Model registry CLI")
            .arg(bucket_arg())
            .arg_required_else_help(true)
            .subcommand_required(true);
        for c in &self.commands {
            cli = cli.subcommand(c.definition());
        }
        cli.subcommand(completion::definition())
    }

    /// `args` includes the program name. Help/usage errors are returned as clap errors.
    pub fn dispatch<I, T>(&self, args: I, out: &mut dyn Write) -> CmdResult
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut cli = self.cli();
        let matches = cli.clone().try_get_matches_from(args)?;
        let (name, sub) = matches.subcommand().ok_or("no command given")?;
        if name == completion::NAME {
            return completion::run(&mut cli, sub, out);
        }
        let cmd = self
            .commands
            .iter()
            .find(|c| c.definition().get_name() == name)
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        cmd.run(sub, out)
    }
}

/// A cargo-style status prefix: a right-aligned bold green verb, e.g. `    Fetched`.
/// Colors are dropped automatically when stdout is not a terminal.
pub fn status(verb: &str) -> String {
    let styled = console::Style::new().green().bold().apply_to(verb);
    format!(
        "{}{styled} ",
        " ".repeat(12usize.saturating_sub(verb.len()))
    )
}

pub const BUCKET_REQUIRED: &str = "bucket is required (use -b/--bucket or set MR_BUCKET)";

/// The bucket option shared by all commands. It is global, so it can be given before or
/// after the command name, and falls back to the `MR_BUCKET` environment variable.
pub fn bucket_arg() -> Arg {
    Arg::new("bucket")
        .short('b')
        .long("bucket")
        .env("MR_BUCKET")
        .global(true)
        .help("S3 bucket name")
}

/// Parses `args` (starting with the command name) the way the real CLI does, so shared
/// global options are available; returns the command's matches.
#[cfg(test)]
pub fn parse_for_test(def: ClapCommand, args: &[&str]) -> Result<ArgMatches, clap::Error> {
    let name = def.get_name().to_string();
    let root = ClapCommand::new("mr").arg(bucket_arg()).subcommand(def);
    let all: Vec<&str> = std::iter::once("mr").chain(args.iter().copied()).collect();
    let m = root.try_get_matches_from(all)?;
    let (n, sub) = m.subcommand().expect("subcommand");
    assert_eq!(n, name);
    Ok(sub.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<String, Box<dyn Error>> {
        let mut out = Vec::new();
        Registry::default_commands().dispatch(args, &mut out)?;
        Ok(String::from_utf8(out)?)
    }

    #[test]
    fn bucket_is_global() {
        let cli = Registry::default_commands().cli();
        for args in [
            ["mr", "-b", "x", "list", "m"],
            ["mr", "list", "m", "-b", "x"],
            ["mr", "list", "m", "--bucket=x", "--full"],
        ] {
            let m = cli.clone().try_get_matches_from(args).unwrap();
            let (_, sub) = m.subcommand().unwrap();
            assert_eq!(sub.get_one::<String>("bucket").unwrap(), "x");
        }
    }

    #[test]
    fn dispatches_version() {
        assert!(run(&["mr", "version"]).unwrap().starts_with("mr version "));
    }

    #[test]
    fn unknown_command_fails() {
        assert!(run(&["mr", "nope"]).is_err());
    }

    #[test]
    fn help_is_reported_as_clap_help() {
        let err = run(&["mr", "--help"]).unwrap_err();
        let e = err.downcast_ref::<clap::Error>().unwrap();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(e.to_string().contains("version"));
    }
}
