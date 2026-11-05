use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, s3_factory};
use crate::models::artifacts::AliasLink;
use crate::models::consts::ARTIFACTS_FILE;
use crate::models::versions::{
    self, LATEST_ALIAS, Tag as TagEntry, validate_version, versions_key,
};
use crate::models::{ModelRef, model_prefix};
use crate::storage::Storage;
use chrono::{SecondsFormat, Utc};
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::Write;

pub struct Tag {
    factory: StorageFactory,
}

impl Tag {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Tag {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Tag {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("tag")
            .about("Point a tag (e.g. production) to a version, or remove a tag")
            .arg(
                Arg::new("model")
                    .value_name("model:version")
                    .help("Model and the version the tag points to (with --delete: model:tag)")
                    .required(true),
            )
            .arg(
                Arg::new("tag")
                    .help("Tag name")
                    .required_unless_present("delete"),
            )
            .arg(
                Arg::new("delete")
                    .short('d')
                    .long("delete")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("tag")
                    .help("Remove the tag"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let (model, version) = ModelRef::parse_with_version(
            args.get_one::<String>("model").ok_or("model is required")?,
        )?;
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let storage = (self.factory)(bucket)?;
        match args.get_one::<String>("tag") {
            Some(tag) => set(storage.as_ref(), &model, tag, &version, out),
            // with --delete the "version" part is the tag to remove
            None => remove(storage.as_ref(), &model, &version, out),
        }
    }
}

fn read(
    storage: &dyn Storage,
    key: &str,
) -> Result<versions::Versions, Box<dyn std::error::Error>> {
    if !storage.exists(key)? {
        return Err(format!("model not initialized: {key} (run `mr init`)").into());
    }
    Ok(versions::parse(&storage.get(key)?).map_err(|e| format!("invalid {key}: {e}"))?)
}

/// The alias folder only holds an artifacts.json link to the real version, no copies.
fn set(
    storage: &dyn Storage,
    model: &str,
    tag: &str,
    version: &str,
    out: &mut dyn Write,
) -> CmdResult {
    validate_version(tag)?;
    if tag == LATEST_ALIAS {
        return Err("'latest' is built in and always points to the newest version".into());
    }
    let prefix = model_prefix(model)?;
    let key = versions_key(model)?;
    let mut list = read(storage, &key)?;
    if list.find(tag).is_some() {
        return Err(format!("'{tag}' is already a version name").into());
    }
    let target = list
        .find(version)
        .ok_or_else(|| format!("version '{version}' not found (aliases cannot point to aliases)"))?
        .clone();
    let link = AliasLink {
        version: target.version.clone(),
        path: target.path.clone(),
    };
    storage.put(
        &format!("{prefix}/{tag}/{ARTIFACTS_FILE}"),
        &serde_json::to_vec_pretty(&link)?,
    )?;

    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let previous = list
        .tags
        .insert(
            tag.to_string(),
            TagEntry {
                version: target.version.clone(),
                created_at,
            },
        )
        .map(|t| t.version);
    storage.put(&key, &serde_json::to_vec_pretty(&list)?)?;
    match previous {
        Some(p) if p != target.version => writeln!(out, "Tag {tag}: {p} -> {version}")?,
        Some(_) => writeln!(out, "Tag {tag} already points to {version}")?,
        None => writeln!(out, "Tag {tag} -> {version}")?,
    }
    Ok(())
}

fn remove(storage: &dyn Storage, model: &str, tag: &str, out: &mut dyn Write) -> CmdResult {
    let prefix = model_prefix(model)?;
    let key = versions_key(model)?;
    let mut list = read(storage, &key)?;
    if list.tags.remove(tag).is_none() {
        return Err(format!("tag '{tag}' not found").into());
    }
    storage.put(&key, &serde_json::to_vec_pretty(&list)?)?;
    storage.delete_prefix(&format!("{prefix}/{tag}/"))?;
    writeln!(out, "Removed tag {tag}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::versions::{Version, Versions};
    use crate::storage::mem::MemStorage;

    fn setup() -> MemStorage {
        let mem = MemStorage::default();
        let mut v = Versions::new("2026-11-05T09:00:00Z".into());
        for n in ["v1", "v2"] {
            v.add(Version {
                version: n.into(),
                tool_version: "0".into(),
                path: format!("m/{n}"),
                created_at: "2026-11-05T10:00:00Z".into(),
                file_count: 1,
                total_size: 1,
                message: String::new(),
            });
            mem.put(
                &format!("m/{n}/artifacts.json"),
                format!(r#"{{"version":"{n}","files":[]}}"#).as_bytes(),
            )
            .unwrap();
        }
        mem.put("m/versions.json", &serde_json::to_vec(&v).unwrap())
            .unwrap();
        mem
    }

    fn list(mem: &MemStorage) -> Versions {
        versions::parse(&mem.get("m/versions.json").unwrap()).unwrap()
    }

    #[test]
    fn sets_moves_and_removes() {
        let mem = setup();
        let mut out = Vec::new();
        set(&mem, "m", "production", "v1", &mut out).unwrap();
        assert_eq!(list(&mem).resolve("production"), Some("v1"));
        let link = |m: &MemStorage| -> AliasLink {
            serde_json::from_slice(&m.get("m/production/artifacts.json").unwrap()).unwrap()
        };
        assert_eq!(
            link(&mem),
            AliasLink {
                version: "v1".into(),
                path: "m/v1".into()
            }
        );

        set(&mem, "m", "production", "v2", &mut out).unwrap();
        assert_eq!(list(&mem).resolve("production"), Some("v2"));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("production: v1 -> v2")
        );
        assert_eq!(link(&mem).version, "v2");

        remove(&mem, "m", "production", &mut Vec::new()).unwrap();
        assert!(list(&mem).tags.is_empty());
        assert!(mem.list("m/production/").unwrap().is_empty());
        assert!(remove(&mem, "m", "production", &mut Vec::new()).is_err());
    }

    #[test]
    fn rejects_bad_aliases() {
        let mem = setup();
        let mut out = Vec::new();
        assert!(set(&mem, "m", "latest", "v1", &mut out).is_err());
        assert!(set(&mem, "m", "v2", "v1", &mut out).is_err());
        assert!(set(&mem, "m", "prod", "nope", &mut out).is_err());
        assert!(set(&mem, "m", "a/b", "v1", &mut out).is_err());
    }

    #[test]
    fn latest_resolves_built_in() {
        let mem = setup();
        assert_eq!(list(&mem).resolve("latest"), Some("v2"));
        assert_eq!(list(&mem).resolve("zzz"), None);
    }
}
