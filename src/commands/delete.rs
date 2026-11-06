use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, s3_factory};
use crate::commands::status;
use crate::models::versions::{self, validate_version, versions_key};
use crate::models::{ModelRef, lock_key, model_prefix};
use crate::storage::Storage;
use crate::storage::lock::with_lock;
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::{BufRead, Write};

pub struct Delete {
    factory: StorageFactory,
}

impl Delete {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Delete {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Delete {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("delete")
            .about("Delete a version of a model from the bucket")
            .arg(
                Arg::new("model")
                    .value_name("model:version")
                    .help("Model and the version to delete")
                    .required(true),
            )
            .arg(
                Arg::new("yes")
                    .short('y')
                    .long("yes")
                    .action(ArgAction::SetTrue)
                    .help("Do not ask for confirmation"),
            )
            .arg(
                Arg::new("dry")
                    .long("dry")
                    .action(ArgAction::SetTrue)
                    .help("Show what would be deleted without changing anything"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let (model, version) = ModelRef::parse_with_version(
            args.get_one::<String>("model").ok_or("model is required")?,
        )?;
        let (model, version) = (model.as_str(), version.as_str());
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let storage = (self.factory)(bucket)?;
        let confirm: &mut dyn FnMut() -> bool = if args.get_flag("yes") {
            &mut || true
        } else {
            &mut || ask(&format!("Delete {model}/{version} from {bucket}? [y/N] "))
        };
        let storage = storage.as_ref();
        let dry = args.get_flag("dry");
        let mut run = move || delete(storage, model, version, dry, confirm, out);
        if dry {
            run()
        } else {
            with_lock(storage, &lock_key(model)?, run)
        }
    }
}

pub(super) fn ask(question: &str) -> bool {
    eprint!("{question}");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn delete(
    storage: &dyn Storage,
    model: &str,
    version: &str,
    dry: bool,
    confirm: &mut dyn FnMut() -> bool,
    out: &mut dyn Write,
) -> CmdResult {
    validate_version(version)?;
    let key = versions_key(model)?;
    if !storage.exists(&key)? {
        return Err(format!("model not initialized: {key} (run `mr init`)").into());
    }
    let mut versions =
        versions::parse(&storage.get(&key)?).map_err(|e| format!("invalid {key}: {e}"))?;
    let entry = versions
        .find(version)
        .ok_or_else(|| format!("version '{version}' not found in {model}"))?
        .clone();
    let aliases = versions.aliases_of(version);
    if !aliases.is_empty() {
        return Err(format!(
            "version '{version}' is used by alias {}; remove the alias first (`mr alias --delete`)",
            aliases.join(", ")
        )
        .into());
    }
    let prefix = format!("{}/{version}/", model_prefix(model)?);

    if dry {
        let n = storage.list(&prefix)?.len();
        writeln!(
            out,
            "[dry] would delete {n} files in {prefix} and remove '{version}' from {key}, nothing was changed"
        )?;
        return Ok(());
    }
    if !confirm() {
        return Err("aborted".into());
    }

    // Unregister first: a half-deleted version must never be listed as available.
    versions.remove(version);
    storage.put(&key, &serde_json::to_vec_pretty(&versions)?)?;
    let n = storage.delete_prefix(&prefix).map_err(|e| {
        format!("'{version}' was unregistered but deleting {prefix} failed: {e}; leftovers can be removed by `mr push --force` or manually")
    })?;
    writeln!(
        out,
        "{}{version}: {n} files ({} bytes) from {prefix}",
        status("Deleted"),
        entry.total_size
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::versions::{Version, Versions};
    use crate::storage::mem::MemStorage;
    use googletest::assert_that;
    use googletest::prelude::*;

    fn entry(v: &str) -> Version {
        Version {
            version: v.into(),
            tool_version: "0".into(),
            path: format!("m/{v}"),
            created_at: "2026-11-05T10:00:00Z".into(),
            file_count: 1,
            total_size: 5,
            message: String::new(),
        }
    }

    fn setup() -> MemStorage {
        let mem = MemStorage::default();
        let mut v = Versions::new("2026-11-05T09:00:00Z".into());
        v.add(entry("v1"));
        v.add(entry("v2"));
        mem.put("m/versions.json", &serde_json::to_vec(&v).unwrap())
            .unwrap();
        for k in ["m/v1/a", "m/v1/artifacts.json", "m/v2/a"] {
            mem.put(k, b"x").unwrap();
        }
        mem
    }

    fn run(mem: &MemStorage, version: &str, dry: bool, yes: bool) -> Result<String, String> {
        let mut out = Vec::new();
        delete(mem, "m", version, dry, &mut || yes, &mut out).map_err(|e| e.to_string())?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn versions(mem: &MemStorage) -> Versions {
        versions::parse(&mem.get("m/versions.json").unwrap()).unwrap()
    }

    #[test]
    fn deletes_version_and_updates_latest() {
        let mem = setup();
        let out = run(&mem, "v2", false, true).unwrap();
        assert_that!(out, contains_substring("Deleted"));
        assert_that!(out, contains_substring("v2: 1 files"));
        assert_eq!(versions(&mem).latest_model_version, "v1");
        assert!(mem.list("m/v2/").unwrap().is_empty());
        assert_eq!(mem.list("m/v1/").unwrap().len(), 2);
    }

    #[test]
    fn deleting_other_keeps_latest() {
        let mem = setup();
        run(&mem, "v1", false, true).unwrap();
        let v = versions(&mem);
        assert_eq!(v.latest_model_version, "v2");
        assert_eq!(v.versions.len(), 1);
    }

    #[test]
    fn dry_and_declined_change_nothing() {
        let mem = setup();
        assert!(
            run(&mem, "v1", true, false)
                .unwrap()
                .contains("[dry] would delete 2 files")
        );
        assert_eq!(run(&mem, "v1", false, false).unwrap_err(), "aborted");
        assert_eq!(versions(&mem).versions.len(), 2);
        assert_eq!(mem.list("m/v1/").unwrap().len(), 2);
    }

    #[test]
    fn refuses_aliased_version() {
        let mem = setup();
        let mut v = versions(&mem);
        v.tags.insert(
            "production".into(),
            crate::models::versions::Tag {
                version: "v1".into(),
                created_at: String::new(),
            },
        );
        mem.put("m/versions.json", &serde_json::to_vec(&v).unwrap())
            .unwrap();
        assert!(
            run(&mem, "v1", false, true)
                .unwrap_err()
                .contains("production")
        );
        assert_eq!(mem.list("m/v1/").unwrap().len(), 2);
    }

    #[test]
    fn unknown_or_invalid_version_fails() {
        let mem = setup();
        assert!(
            run(&mem, "v9", false, true)
                .unwrap_err()
                .contains("not found")
        );
        assert!(run(&mem, "a/b", false, true).is_err());
    }
}
