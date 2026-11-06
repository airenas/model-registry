use super::{CmdResult, Command, StorageFactory, open_storage, s3_factory};
use crate::models::index;
use crate::models::versions::{self, LATEST_ALIAS, versions_key};
use crate::storage::Storage;
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::Write;

/// Hidden helper called by the installed shell completion: prints candidates for the word
/// being typed, one per line as `value<TAB>description`.
pub struct Complete {
    factory: StorageFactory,
}

impl Complete {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Complete {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Complete {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("complete")
            .hide(true)
            .about("Print completion candidates for a model[:version] word")
            .arg(Arg::new("word").default_value(""))
            .arg(
                Arg::new("models")
                    .long("models")
                    .action(ArgAction::SetTrue)
                    .help("Only complete model names"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let storage = open_storage(&self.factory, args)?;
        let word = args
            .get_one::<String>("word")
            .map(String::as_str)
            .unwrap_or("");
        candidates(
            storage.as_ref(),
            super::group(args),
            word,
            args.get_flag("models"),
            out,
        )
    }
}

/// `group/model`, or just `group/` for an empty model; the model itself without a group.
fn with_group(group: &str, model: &str) -> String {
    if group.is_empty() {
        model.to_string()
    } else {
        format!("{group}/{model}")
    }
}

fn candidates(
    storage: &dyn Storage,
    group: &str,
    word: &str,
    models_only: bool,
    out: &mut dyn Write,
) -> CmdResult {
    match word.split_once(':') {
        Some((model, part)) if !models_only => {
            let key = versions_key(&with_group(group, model))?;
            if !storage.exists(&key)? {
                return Ok(());
            }
            let list = versions::parse(&storage.get(&key)?)?;
            let mut items: Vec<(String, String)> = list
                .versions
                .iter()
                .map(|v| (v.version.clone(), v.message.clone()))
                .collect();
            items.extend(
                list.tags
                    .iter()
                    .map(|(t, tag)| (t.clone(), format!("tag -> {}", tag.version))),
            );
            if !list.latest_model_version.is_empty() {
                items.push((
                    LATEST_ALIAS.into(),
                    format!("tag -> {}", list.latest_model_version),
                ));
            }
            for (name, desc) in items.into_iter().filter(|(n, _)| n.starts_with(part)) {
                writeln!(out, "{model}:{name}\t{}", desc.replace(['\n', '\t'], " "))?;
            }
        }
        _ => {
            let key = index::models_key();
            if storage.exists(key)? {
                for m in index::parse(&storage.get(key)?)?.models {
                    let name = m.name.strip_prefix(&with_group(group, "")).unwrap_or("");
                    if !name.is_empty() && name.starts_with(word) {
                        writeln!(out, "{name}:")?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mem::MemStorage;

    fn mem() -> MemStorage {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v2","tags":{"prod":{"version":"v1","created_at":"x"}},"versions":[
              {"version":"v1","tool_version":"0.1.0","path":"m1/v1","created_at":"x","file_count":0,"total_size":0,"message":"first"},
              {"version":"v2","tool_version":"0.1.0","path":"m1/v2","created_at":"x","file_count":0,"total_size":0}]}"#,
        )
        .unwrap();
        mem.put(
            "models.json",
            br#"{"tool_version":"0.1.0","created_at":"x","models":[
              {"name":"m1","created_at":"x"},{"name":"m2","created_at":"x"},{"name":"tts/me/m1","created_at":"x"}]}"#,
        )
        .unwrap();
        mem
    }

    fn run(word: &str, models_only: bool) -> String {
        let mut out = Vec::new();
        candidates(&mem(), "", word, models_only, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn completes_models() {
        assert_eq!(run("", false), "m1:\nm2:\ntts/me/m1:\n");
        assert_eq!(run("m1", false), "m1:\n");
        assert_eq!(run("x", false), "");
        assert_eq!(run("t", false), "tts/me/m1:\n");
        assert_eq!(run("tts/me/", false), "tts/me/m1:\n");
    }

    #[test]
    fn completes_versions_and_tags() {
        assert_eq!(
            run("m1:", false),
            "m1:v1\tfirst\nm1:v2\t\nm1:prod\ttag -> v1\nm1:latest\ttag -> v2\n"
        );
        assert_eq!(run("m1:v", false), "m1:v1\tfirst\nm1:v2\t\n");
        assert_eq!(run("nope:", false), "");
    }

    #[test]
    fn completes_within_group() {
        let mut out = Vec::new();
        candidates(&mem(), "tts/me", "", false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "m1:\n");
        let mem = mem();
        mem.put(
            "tts/me/m1/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v1","versions":[
              {"version":"v1","tool_version":"0.1.0","path":"p","created_at":"x","file_count":0,"total_size":0}]}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        candidates(&mem, "tts/me", "m1:", false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "m1:v1\t\nm1:latest\ttag -> v1\n"
        );
    }
}
