use super::{CmdResult, Command};
use clap::{ArgMatches, Command as ClapCommand};
use std::io::Write;

pub struct Version {
    version: &'static str,
}

impl Version {
    pub fn new() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    pub fn with_version(version: &'static str) -> Self {
        Self { version }
    }
}

impl Default for Version {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Version {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("version").about("Print the tool version")
    }

    fn run(&self, _args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        writeln!(out, "mr version {}", self.version)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_version() {
        let mut out = Vec::new();
        Version::with_version("1.2.3")
            .run(&ArgMatches::default(), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "mr version 1.2.3\n");
    }
}
