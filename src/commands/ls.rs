use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, s3_factory};
use crate::models::consts::ARTIFACTS_FILE;
use crate::models::index::{self, ModelEntry};
use crate::models::versions::{self, LATEST_ALIAS, Versions, versions_key};
use crate::models::{ModelRef, artifacts};
use crate::storage::Storage;
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::Write;

pub struct List {
    factory: StorageFactory,
}

impl List {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for List {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for List {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("ls")
            .about("List the models in the bucket; `model` lists its versions, `model:version` shows that version")
            .arg(
                Arg::new("model")
                    .value_name("model[:version]")
                    .help(
                        "Model name (folder in the bucket) to list its versions, or \
                         model:version (a tag or `latest` also works) to show one version",
                    )
                    .required(false),
            )
            .arg(
                Arg::new("full")
                    .long("full")
                    .short('f')
                    .action(ArgAction::SetTrue)
                    .help("Also show path, file count and total size"),
            )
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let storage = (self.factory)(bucket)?;
        let Some(target) = args.get_one::<String>("model") else {
            return list_models(storage.as_ref(), bucket, args.get_flag("full"), out);
        };
        let ModelRef { model, version } = ModelRef::parse(target)?;
        let model = &model;
        let key = versions_key(model)?;
        if !storage.exists(&key)? {
            return Err(
                format!("model not initialized: s3://{bucket}/{key} (run `mr init`)").into(),
            );
        }
        let versions = versions::parse(&storage.get(&key)?)
            .map_err(|e| format!("invalid s3://{bucket}/{key}: {e}"))?;
        if let Some(name) = version {
            return show_version(
                storage.as_ref(),
                model,
                &name,
                &versions,
                args.get_flag("full"),
                out,
            );
        }
        if versions.versions.is_empty() {
            if !versions.description.is_empty() {
                writeln!(
                    out,
                    "{} {}",
                    lbl("Description  :"),
                    one_line(&versions.description)
                )?;
            }
            writeln!(out, "No versions")?;
            return Ok(());
        }
        let full = args.get_flag("full");
        let mut rows = vec![if full {
            vec![
                "VERSION",
                "CREATED",
                "FILES",
                "SIZE",
                "PATH",
                "TOOL VERSION",
                "MESSAGE",
            ]
            .into_iter()
            .map(String::from)
            .collect()
        } else {
            vec!["VERSION".to_string(), "CREATED".to_string()]
        }];
        let with_message = !full && versions.versions.iter().any(|v| !v.message.is_empty());
        if with_message {
            rows[0].push("MESSAGE".to_string());
        }
        for v in &versions.versions {
            let mut row = vec![v.version.clone(), v.created_at.clone()];
            if full {
                row.extend([
                    v.file_count.to_string(),
                    human_size(v.total_size),
                    v.path.clone(),
                    v.tool_version.clone(),
                    one_line(&v.message),
                ]);
            } else if with_message {
                row.push(one_line(&v.message));
            }
            rows.push(row);
        }
        writeln!(out, "{} {}", lbl("Model        :"), model)?;
        writeln!(out, "{} {}", lbl("Tool version :"), versions.tool_version)?;
        writeln!(out, "{} {}", lbl("Created at   :"), versions.created_at)?;
        writeln!(
            out,
            "{} {}",
            lbl("Latest       :"),
            versions.latest_model_version
        )?;
        if !versions.description.is_empty() {
            writeln!(
                out,
                "{} {}",
                lbl("Description  :"),
                one_line(&versions.description)
            )?;
        }

        writeln!(out)?;
        write_table_with(out, &rows, Some(console::Style::new().bold()))?;
        if !versions.tags.is_empty() {
            let mut tags = vec![["TAG", "VERSION", "CREATED"].map(String::from).to_vec()];
            for (name, t) in &versions.tags {
                tags.push(vec![name.clone(), t.version.clone(), t.created_at.clone()]);
            }
            writeln!(out)?;
            write_table_with(out, &tags, Some(console::Style::new().bold()))?;
        }
        Ok(())
    }
}

/// Details of one version: its entry in versions.json plus the files from its artifacts.json.
fn show_version(
    storage: &dyn Storage,
    model: &str,
    name: &str,
    versions: &Versions,
    full: bool,
    out: &mut dyn Write,
) -> CmdResult {
    let resolved = versions
        .resolve(name)
        .ok_or_else(|| format!("version or tag '{name}' not found in {model}"))?;
    let entry = versions.find(resolved).ok_or("version not found")?;
    let akey = format!("{}/{ARTIFACTS_FILE}", entry.path);
    let artifacts =
        artifacts::parse(&storage.get(&akey)?).map_err(|e| format!("invalid {akey}: {e}"))?;

    writeln!(out, "{} {model}", lbl("Model        :"))?;
    if resolved == name {
        writeln!(out, "{} {}", lbl("Version      :"), entry.version)?;
    } else {
        writeln!(out, "{} {} ({name})", lbl("Version      :"), entry.version)?;
    }
    writeln!(out, "{} {}", lbl("Created at   :"), entry.created_at)?;
    writeln!(out, "{} {}", lbl("Tool version :"), entry.tool_version)?;
    writeln!(out, "{} {}", lbl("Path         :"), entry.path)?;
    let mut tags = versions.aliases_of(resolved);
    if versions.latest_model_version == resolved {
        tags.insert(0, LATEST_ALIAS);
    }
    if !tags.is_empty() {
        writeln!(out, "{} {}", lbl("Tags         :"), tags.join(", "))?;
    }
    if !entry.message.is_empty() {
        writeln!(
            out,
            "{} {}",
            lbl("Message      :"),
            one_line(&entry.message)
        )?;
    }
    writeln!(
        out,
        "{} {} ({})",
        lbl("Files        :"),
        artifacts.files.len(),
        human_size(artifacts.files.iter().map(|f| f.size).sum())
    )?;
    if artifacts.files.is_empty() {
        return Ok(());
    }
    let mut rows = vec![if full {
        ["FILE", "SIZE", "MODIFIED", "BLAKE3"]
            .map(String::from)
            .to_vec()
    } else {
        ["FILE", "SIZE", "MODIFIED"].map(String::from).to_vec()
    }];
    for f in &artifacts.files {
        let mut row = vec![f.path.clone(), human_size(f.size), f.modified_at.clone()];
        if full {
            row.push(f.blake3.clone());
        }
        rows.push(row);
    }
    writeln!(out)?;
    write_table_with(out, &rows, Some(console::Style::new().italic()))?;
    Ok(())
}

/// Models from the bucket's models.json. Buckets without one (initialized by older
/// versions) fall back to scanning for top-level folders that contain a versions.json.
fn list_models(storage: &dyn Storage, bucket: &str, full: bool, out: &mut dyn Write) -> CmdResult {
    let key = index::models_key();
    let entries = if storage.exists(key)? {
        index::parse(&storage.get(key)?)
            .map_err(|e| format!("invalid {key}: {e}"))?
            .models
    } else {
        scan_models(storage)?
    };
    if entries.is_empty() {
        writeln!(out, "No models in {bucket}")?;
        return Ok(());
    }
    if !full {
        for e in &entries {
            writeln!(out, "{}", e.name)?;
        }
        return Ok(());
    }
    let mut rows = vec![
        ["MODEL", "CREATED", "DESCRIPTION"]
            .map(String::from)
            .to_vec(),
    ];
    for e in &entries {
        rows.push(vec![
            e.name.clone(),
            e.created_at.clone(),
            one_line(&e.description),
        ]);
    }
    write_table_with(out, &rows, Some(console::Style::new().bold()))?;
    Ok(())
}

fn scan_models(storage: &dyn Storage) -> Result<Vec<ModelEntry>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for name in storage.list_dirs("")? {
        let key = versions_key(&name)?;
        if !storage.exists(&key)? {
            continue;
        }
        let (created_at, description) = match versions::parse(&storage.get(&key)?) {
            Ok(v) => (v.created_at, v.description),
            Err(_) => (String::new(), "(invalid versions.json)".into()),
        };
        entries.push(ModelEntry {
            name,
            created_at,
            description,
        });
    }
    Ok(entries)
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Binary units, e.g. 1.5 GiB; bytes are shown as-is.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
/// Grey text; `console` disables colors when stdout is not a terminal, or on NO_COLOR / TERM=dumb.
fn grey(text: &str) -> String {
    console::Style::new()
        .color256(250)
        .apply_to(text)
        .to_string()
}

fn lbl(label: &str) -> String {
    grey(label)
}

/// With `first_col` set, the first column of the body rows is styled (padding stays unstyled).
fn write_table_with(
    out: &mut dyn Write,
    rows: &[Vec<String>],
    first_col: Option<console::Style>,
) -> std::io::Result<()> {
    let cols = rows[0].len();
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0))
        .collect();
    for (n, r) in rows.iter().enumerate() {
        let first = n == 0;
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let pad = if i + 1 == cols {
                    0
                } else {
                    widths[i] - cell.len()
                };
                let text = match &first_col {
                    Some(style) if !first && i == 0 => style.apply_to(cell).to_string(),
                    _ => cell.clone(),
                };
                format!("{text}{}", " ".repeat(pad))
            })
            .collect();
        if first {
            writeln!(out, "{}", grey(&line.join("  ")))?;
        } else {
            writeln!(out, "{}", line.join("  "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Storage, mem::MemStorage};

    fn run(mem: &MemStorage) -> Result<String, Box<dyn std::error::Error>> {
        run_args(mem, &[])
    }

    fn run_args(mem: &MemStorage, extra: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
        let m = mem.clone();
        let cmd = List::with_factory(Box::new(move |_| {
            Ok(Box::new(m.clone()) as Box<dyn Storage>)
        }));
        let args = crate::commands::parse_for_test(
            cmd.definition(),
            &[&["list", "m1", "-b", "b"][..], extra].concat(),
        )
        .unwrap();
        let mut out = Vec::new();
        cmd.run(&args, &mut out)?;
        Ok(String::from_utf8(out)?)
    }

    fn versions_json(entries: &str) -> Vec<u8> {
        format!(
            r#"{{"tool_version":"0.1.0","created_at":"2026-11-05T09:00:00Z","latest_model_version":"","versions":[{entries}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn lists_versions() {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            &versions_json(
                r#"{"version":"v1","tool_version":"0.1.0","path":"m1/v1","created_at":"2026-11-05T09:59:00Z","file_count":1,"total_size":2},
                   {"version":"v10","tool_version":"0.1.0","path":"m1/v10","created_at":"2026-11-06T10:00:00Z","file_count":1,"total_size":2}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            run(&mem).unwrap(),
            "Model        : m1\nTool version : 0.1.0\nCreated at   : 2026-11-05T09:00:00Z\nLatest       : \n\nVERSION  CREATED\nv1       2026-11-05T09:59:00Z\nv10      2026-11-06T10:00:00Z\n"
        );
    }

    #[test]
    fn lists_full() {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            &versions_json(
                r#"{"version":"v1","tool_version":"0.1.0","path":"m1/v1","created_at":"2026-11-05T09:59:00Z","file_count":3,"total_size":1610612736}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            run_args(&mem, &["--full"]).unwrap(),
            "Model        : m1\nTool version : 0.1.0\nCreated at   : 2026-11-05T09:00:00Z\nLatest       : \n\nVERSION  CREATED               FILES  SIZE     PATH   TOOL VERSION  MESSAGE\nv1       2026-11-05T09:59:00Z  3      1.5 GiB  m1/v1  0.1.0         \n"
        );
    }

    #[test]
    fn shows_messages() {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            &versions_json(
                r#"{"version":"v1","tool_version":"0.1.0","path":"m1/v1","created_at":"2026-11-05T09:59:00Z","file_count":1,"total_size":2,"message":"changed model type"},
                   {"version":"v2","tool_version":"0.1.0","path":"m1/v2","created_at":"2026-11-06T10:00:00Z","file_count":1,"total_size":2}"#,
            ),
        )
        .unwrap();
        let out = run(&mem).unwrap();
        assert!(out.contains("VERSION  CREATED               MESSAGE\n"));
        assert!(out.contains("v1       2026-11-05T09:59:00Z  changed model type\n"));
        let full = run_args(&mem, &["--full"]).unwrap();
        assert!(full.contains("0.1.0         changed model type\n"));
    }

    #[test]
    fn shows_description() {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"2026-11-05T09:00:00Z","latest_model_version":"","description":"speech tagger","versions":[]}"#,
        )
        .unwrap();
        assert_eq!(
            run(&mem).unwrap(),
            "Description  : speech tagger\nNo versions\n"
        );
    }

    #[test]
    fn lists_models() {
        let mem = MemStorage::default();
        let init = |name: &str, desc: &str| {
            mem.put(
                &format!("{name}/versions.json"),
                format!(
                    r#"{{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v2","description":"{desc}","versions":[{{"version":"v1","tool_version":"0","path":"{name}/v1","created_at":"x","file_count":1,"total_size":1}}]}}"#
                )
                .as_bytes(),
            )
            .unwrap();
        };
        init("tagger", "pos tagger");
        init("asr", "");
        mem.put("junk/file.bin", b"x").unwrap();
        mem.put("tagger/v1/a", b"x").unwrap();
        let args = |a: &[&str]| {
            let cmd = List::with_factory({
                let m = mem.clone();
                Box::new(move |_| Ok(Box::new(m.clone()) as Box<dyn Storage>))
            });
            let m = crate::commands::parse_for_test(cmd.definition(), a).unwrap();
            let mut out = Vec::new();
            cmd.run(&m, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(args(&["list", "-b", "b"]), "asr\ntagger\n");
        let full = args(&["list", "-b", "b", "--full"]);
        assert!(full.starts_with("MODEL   CREATED  DESCRIPTION\n"));
        assert!(full.contains("tagger  x        pos tagger\n"));
    }

    #[test]
    fn lists_models_from_index() {
        let mem = MemStorage::default();
        mem.put(
            "models.json",
            br#"{"tool_version":"0.1.0","created_at":"t","models":[
              {"name":"a","created_at":"2026-11-05T10:00:00Z","description":"first"},
              {"name":"b","created_at":"2026-11-06T10:00:00Z"}]}"#,
        )
        .unwrap();
        // folders without an entry in the index are not listed
        mem.put("c/versions.json", b"{}").unwrap();
        let mut out = Vec::new();
        list_models(&mem, "b", false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a\nb\n");
        let mut out = Vec::new();
        list_models(&mem, "b", true, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("a      2026-11-05T10:00:00Z  first\n"),
            "{out}"
        );
    }

    #[test]
    fn no_models() {
        let mem = MemStorage::default();
        mem.put("junk/file.bin", b"x").unwrap();
        let mut out = Vec::new();
        list_models(&mem, "b", false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No models in b\n");
    }

    #[test]
    fn shows_one_version() {
        let mem = MemStorage::default();
        mem.put(
            "m1/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v2","tags":{"prod":"v1"},"versions":[
              {"version":"v1","tool_version":"0.1.0","path":"m1/v1","created_at":"2026-11-05T09:59:00Z","file_count":2,"total_size":1536,"message":"first"},
              {"version":"v2","tool_version":"0.1.0","path":"m1/v2","created_at":"2026-11-06T10:00:00Z","file_count":0,"total_size":0}]}"#,
        )
        .unwrap();
        mem.put(
            "m1/v1/artifacts.json",
            br#"{"version":"v1","files":[
              {"path":"a.bin","size":1024,"modified_at":"2026-11-05T09:00:00Z","blake3":"abc"},
              {"path":"sub/b.txt","size":512,"modified_at":"2026-11-05T09:01:00Z","blake3":"def"}]}"#,
        )
        .unwrap();
        mem.put("m1/v2/artifacts.json", br#"{"version":"v2","files":[]}"#)
            .unwrap();
        let out = run_args_model(&mem, "m1:prod", &[]).unwrap();
        assert!(out.contains("Version      : v1 (prod)\n"));
        assert!(out.contains("Tags         : prod\n"));
        assert!(out.contains("Message      : first\n"));
        assert!(out.contains("Files        : 2 (1.5 KiB)\n"));
        assert!(out.contains("a.bin      1.0 KiB  2026-11-05T09:00:00Z\n"));
        assert!(!out.contains("abc"));
        assert!(
            run_args_model(&mem, "m1:v1", &["--full"])
                .unwrap()
                .contains("abc")
        );
        assert!(
            run_args_model(&mem, "m1:latest", &[])
                .unwrap()
                .contains("Version      : v2 (latest)\nCreated at   : 2026-11-06T10:00:00Z")
        );
        assert!(
            run_args_model(&mem, "m1:v2", &[])
                .unwrap()
                .contains("Tags         : latest\n")
        );
        assert!(run_args_model(&mem, "m1:nope", &[]).is_err());
    }

    fn run_args_model(
        mem: &MemStorage,
        target: &str,
        extra: &[&str],
    ) -> Result<String, Box<dyn std::error::Error>> {
        let cmd = List::with_factory({
            let m = mem.clone();
            Box::new(move |_| Ok(Box::new(m.clone()) as Box<dyn Storage>))
        });
        let args = crate::commands::parse_for_test(
            cmd.definition(),
            &[&["list", target, "-b", "b"][..], extra].concat(),
        )?;
        let mut out = Vec::new();
        cmd.run(&args, &mut out)?;
        Ok(String::from_utf8(out)?)
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn empty_list() {
        let mem = MemStorage::default();
        mem.put("m1/versions.json", &versions_json("")).unwrap();
        assert_eq!(run(&mem).unwrap(), "No versions\n");
    }

    #[test]
    fn not_initialized() {
        assert!(
            run(&MemStorage::default())
                .unwrap_err()
                .to_string()
                .contains("not initialized")
        );
    }

    #[test]
    fn invalid_json() {
        let mem = MemStorage::default();
        mem.put("m1/versions.json", b"{").unwrap();
        assert!(run(&mem).unwrap_err().to_string().contains("invalid"));
    }
}
