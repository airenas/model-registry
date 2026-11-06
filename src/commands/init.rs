use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, open_storage, s3_factory};
use crate::commands::status;
use crate::models::consts::MODELS_LOCK;
use crate::models::index::{self, ModelEntry, ModelsIndex};
use crate::models::versions::{Versions, versions_key};
use crate::models::{lock_key, model_prefix};
use crate::storage::Storage;
use crate::storage::lock::with_lock;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Arg, ArgMatches, Command as ClapCommand};
use std::io::Write;

pub struct Init {
    factory: StorageFactory,
}

impl Init {
    /// Uses S3.
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Init {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Init {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("init")
            .about("Initialize a model folder in an S3 bucket")
            .arg(
                Arg::new("model")
                    .help("Model name, used as the folder (key prefix) in the bucket")
                    .required(true),
            )
            .arg(
                Arg::new("description")
                    .short('d')
                    .long("description")
                    .required(true)
                    .help("Description of the model"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let model = &super::model_arg(args)?;
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let key = versions_key(model)?;
        let storage = open_storage(&self.factory, args)?;
        let description = args
            .get_one::<String>("description")
            .map(|m| m.trim())
            .unwrap_or_default();
        let storage = storage.as_ref();
        with_lock(storage, &lock_key(model)?, || {
            init(storage, model, &key, description, Utc::now())
        })?;
        writeln!(out, "{}: s3://{bucket}/{key}", status("Initialized model"))?;
        Ok(())
    }
}

fn init(
    storage: &dyn Storage,
    model: &str,
    key: &str,
    description: &str,
    now: DateTime<Utc>,
) -> CmdResult {
    if storage.exists(key)? {
        return Err("already initialized".into());
    }
    let created_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut versions = Versions::new(created_at.clone());
    versions.description = description.to_string();
    storage.put(key, &serde_json::to_vec_pretty(&versions)?)?;

    with_lock(storage, MODELS_LOCK, || {
        let index_key = index::models_key();
        let mut models = if storage.exists(index_key)? {
            index::parse(&storage.get(index_key)?)
                .map_err(|e| format!("invalid {index_key}: {e}"))?
        } else {
            ModelsIndex::new(created_at.clone())
        };
        models.upsert(ModelEntry {
            name: model_prefix(model)?,
            created_at,
            description: description.to_string(),
        });
        storage.put(index_key, &serde_json::to_vec_pretty(&models)?)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::versions;
    use crate::storage::mem::MemStorage;

    fn cmd(mem: &MemStorage) -> Init {
        let mem = mem.clone();
        Init::with_factory(Box::new(move |_| {
            Ok(Box::new(mem.clone()) as Box<dyn Storage>)
        }))
    }

    fn matches(model: &str) -> ArgMatches {
        crate::commands::parse_for_test(
            cmd(&MemStorage::default()).definition(),
            &["init", model, "--bucket", "bucket", "-d", "desc"],
        )
        .unwrap()
    }

    #[test]
    fn writes_versions_file() {
        let mem = MemStorage::default();
        let mut out = Vec::new();
        cmd(&mem).run(&matches("m1"), &mut out).unwrap();
        let v = versions::parse(&mem.get("m1/versions.json").unwrap()).unwrap();
        assert!(v.versions.is_empty());
        assert_eq!(v.latest_model_version, "");
        assert_eq!(v.tool_version, env!("CARGO_PKG_VERSION"));
        assert!(!v.created_at.is_empty());
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("s3://bucket/m1/versions.json")
        );
    }

    #[test]
    fn stores_description() {
        let mem = MemStorage::default();
        let args = crate::commands::parse_for_test(
            cmd(&mem).definition(),
            &["init", "m1", "-b", "bucket", "-d", "speech tagger"],
        )
        .unwrap();
        cmd(&mem).run(&args, &mut Vec::new()).unwrap();
        let v = versions::parse(&mem.get("m1/versions.json").unwrap()).unwrap();
        assert_eq!(v.description, "speech tagger");
    }

    #[test]
    fn registers_in_models_json() {
        let mem = MemStorage::default();
        let c = cmd(&mem);
        c.run(&matches("b"), &mut Vec::new()).unwrap();
        c.run(&matches("a"), &mut Vec::new()).unwrap();
        let idx = index::parse(&mem.get("models.json").unwrap()).unwrap();
        let names: Vec<_> = idx.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(idx.models[0].description, "desc");
    }

    #[test]
    fn fails_if_already_initialized() {
        let mem = MemStorage::default();
        let c = cmd(&mem);
        c.run(&matches("b"), &mut Vec::new()).unwrap();
        assert!(c.run(&matches("b"), &mut Vec::new()).is_err());
    }

    #[test]
    fn group_is_a_key_prefix() {
        let mem = MemStorage::default();
        let args = crate::commands::parse_for_test(
            cmd(&mem).definition(),
            &["init", "m1", "-b", "bucket", "-g", "tts/sab", "-d", "d"],
        )
        .unwrap();
        let mut out = Vec::new();
        cmd(&mem).run(&args, &mut out).unwrap();
        assert!(mem.exists("tts/sab/m1/versions.json").unwrap());
        let idx = index::parse(&mem.get("models.json").unwrap()).unwrap();
        assert_eq!(idx.models[0].name, "tts/sab/m1");
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("s3://bucket/tts/sab/m1/versions.json")
        );
    }
}
