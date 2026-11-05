use super::{CmdResult, Command};
use crate::storage::{
    StorageResult,
    s3::{self, ConnectionInfo},
};
use clap::{ArgMatches, Command as ClapCommand};
use std::io::Write;

type InfoProvider = Box<dyn Fn(Option<&str>) -> StorageResult<ConnectionInfo>>;

pub struct Info {
    provider: InfoProvider,
}

impl Info {
    pub fn new() -> Self {
        Self::with_provider(Box::new(s3::connection_info))
    }

    pub fn with_provider(provider: InfoProvider) -> Self {
        Self { provider }
    }
}

impl Default for Info {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Info {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("info").about("Print the AWS connection info mr will use")
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let bucket = args.get_one::<String>("bucket").map(String::as_str);
        let info = (self.provider)(bucket)?;
        write!(out, "{info}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_masked_info() {
        let cmd = Info::with_provider(Box::new(|b| {
            assert_eq!(b, Some("bkt"));
            Ok(ConnectionInfo {
                region: Some("eu-west-1".into()),
                access_key_id: Some("AKIA1234567890ABCDEF".into()),
                bucket: Some("bkt".into()),
                bucket_check: Some(Ok(())),
                ..Default::default()
            })
        }));
        let m = crate::commands::parse_for_test(cmd.definition(), &["info", "-b", "bkt"]).unwrap();
        let mut out = Vec::new();
        cmd.run(&m, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("eu-west-1"));
        assert!(s.contains("AKIA...CDEF"));
        assert!(!s.contains("1234567890"));
        assert!(s.contains("Bucket:        bkt (accessible)"));
    }
}
