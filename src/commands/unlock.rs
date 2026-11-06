use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, s3_factory};
use crate::commands::status;
use crate::models::consts::MODELS_LOCK;
use crate::models::lock_key;
use crate::storage::Storage;
use crate::storage::lock::{force_release, peek};
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::Write;

pub struct Unlock {
    factory: StorageFactory,
}

impl Unlock {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Unlock {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Unlock {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("unlock")
            .about("Remove a stale lock of a model (or of models.json when no model is given)")
            .arg(
                Arg::new("model")
                    .value_name("model")
                    .help("Model whose lock to remove"),
            )
            .arg(
                Arg::new("yes")
                    .short('y')
                    .long("yes")
                    .action(ArgAction::SetTrue)
                    .help("Do not ask for confirmation"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let storage = (self.factory)(bucket)?;
        let (name, key) = match args.get_one::<String>("model") {
            Some(m) => (m.clone(), lock_key(m)?),
            None => ("models.json".to_string(), MODELS_LOCK.to_string()),
        };
        let ask: &dyn Fn(&str) -> bool = if args.get_flag("yes") {
            &|_| true
        } else {
            &super::delete::ask
        };
        unlock(storage.as_ref(), &name, &key, ask, out)
    }
}

fn unlock(
    storage: &dyn Storage,
    name: &str,
    key: &str,
    ask: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> CmdResult {
    let Some((owner, age)) = peek(storage, key) else {
        writeln!(out, "{name} is not locked")?;
        return Ok(());
    };
    let q = format!(
        "{name} is locked by {owner} ({age}s ago). Removing the lock of a running command \
         can cause lost updates. Remove it? [y/N] "
    );
    if !ask(&q) {
        return Err("aborted".into());
    }
    force_release(storage, key)?;
    writeln!(out, "{}lock of {name}", status("Unlocked"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mem::MemStorage;

    fn put_lock(mem: &MemStorage, key: &str) {
        mem.put(
            key,
            br#"{"id":"x","owner":"bob@pc (pid 1)","acquired_at":1}"#,
        )
        .unwrap();
    }

    fn run(mem: &MemStorage, extra: &[&str], answer: bool) -> (CmdResult, String) {
        let m = mem.clone();
        let cmd = Unlock::with_factory(Box::new(move |_| {
            Ok(Box::new(m.clone()) as Box<dyn Storage>)
        }));
        let args = crate::commands::parse_for_test(
            cmd.definition(),
            &[&["unlock", "-b", "b"][..], extra].concat(),
        )
        .unwrap();
        let (name, key) = match args.get_one::<String>("model") {
            Some(m) => (m.clone(), lock_key(m).unwrap()),
            None => ("models.json".to_string(), MODELS_LOCK.to_string()),
        };
        let mut out = Vec::new();
        let r = unlock(mem, &name, &key, &|_| answer, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn removes_after_confirmation() {
        let mem = MemStorage::default();
        put_lock(&mem, "m1/.lock");
        let (r, out) = run(&mem, &["m1"], true);
        r.unwrap();
        assert!(out.contains("Unlocked"), "{out}");
        assert!(!mem.exists("m1/.lock").unwrap());
    }

    #[test]
    fn keeps_lock_when_declined() {
        let mem = MemStorage::default();
        put_lock(&mem, "m1/.lock");
        let (r, _) = run(&mem, &["m1"], false);
        assert_eq!(r.unwrap_err().to_string(), "aborted");
        assert!(mem.exists("m1/.lock").unwrap());
    }

    #[test]
    fn not_locked() {
        let mem = MemStorage::default();
        let (r, out) = run(&mem, &["m1"], true);
        r.unwrap();
        assert!(out.contains("m1 is not locked"), "{out}");
    }

    #[test]
    fn unlocks_models_index() {
        let mem = MemStorage::default();
        put_lock(&mem, MODELS_LOCK);
        run(&mem, &[], true).0.unwrap();
        assert!(!mem.exists(MODELS_LOCK).unwrap());
    }
}
