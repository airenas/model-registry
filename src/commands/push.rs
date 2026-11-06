use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, jobs_arg, s3_factory, status};
use crate::models::artifacts::{Artifact, Artifacts};
use crate::models::consts::TOOL_VERSION;
use crate::models::consts::{ARTIFACTS_FILE, LOCAL_DIR};
use crate::models::versions::{AUTO_VERSION, Version, Versions, validate_version, versions_key};
use crate::models::{ModelRef, versions};
use crate::parallel;
use crate::progress::{Progress, speed};
use crate::storage::{self, Storage};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Push {
    factory: StorageFactory,
}

impl Push {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Push {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Push {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("push")
            .about("Upload a directory as a new version of a model")
            .arg(
                Arg::new("model")
                    .value_name("model:version")
                    .help(
                        "Model and the new version, e.g. m1:v1.2.0; use m1:auto to increase \
                         the latest version (v0.0.1 for the first push)",
                    )
                    .required(true),
            )
            .arg(
                Arg::new("dir")
                    .help("Local directory to upload, including subdirectories")
                    .value_parser(clap::value_parser!(PathBuf))
                    .required(true),
            )
            .arg(
                Arg::new("dry")
                    .long("dry")
                    .action(ArgAction::SetTrue)
                    .help("Show what would be uploaded without writing anything"),
            )
            .arg(
                Arg::new("force")
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help("Delete leftover files of an interrupted push of this version and retry"),
            )
            .arg(
                Arg::new("message")
                    .short('m')
                    .long("message")
                    .help("Note describing what changed in this version"),
            )
            .arg(
                Arg::new("yes")
                    .short('y')
                    .long("yes")
                    .action(ArgAction::SetTrue)
                    .help("Do not ask for confirmation when the version name gets a warning"),
            )
            .arg(jobs_arg())
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let (model, version) = ModelRef::parse_with_version(
            args.get_one::<String>("model").ok_or("model is required")?,
        )
        .map_err(|e| format!("{e}; use model:auto for the next version"))?;
        let version = version.as_str();
        let dir = args.get_one::<PathBuf>("dir").ok_or("dir is required")?;
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;
        let opts = Options {
            message: args
                .get_one::<String>("message")
                .map(|m| m.trim().to_string())
                .unwrap_or_default(),
            dry: args.get_flag("dry"),
            force: args.get_flag("force"),
            yes: args.get_flag("yes"),
            ask: super::delete::ask,
            jobs: (*args.get_one::<usize>("jobs").ok_or("jobs is required")?).clamp(1, 64),
        };

        // validate before connecting to S3
        if version != AUTO_VERSION {
            validate_version(version)?;
        }
        let files = scan(dir)?;
        let storage = (self.factory)(bucket)?;
        push(
            storage.as_ref(),
            &model,
            version,
            files,
            opts,
            Utc::now(),
            out,
        )
    }
}

#[derive(Debug, Clone)]
struct Options {
    message: String,
    dry: bool,
    force: bool,
    /// Skip the question after version warnings.
    yes: bool,
    ask: fn(&str) -> bool,
    jobs: usize,
}

/// A local file with its computed metadata.
struct LocalFile {
    abs: PathBuf,
    artifact: Artifact,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Walks the directory (following symlinks) and hashes every file.
fn scan(dir: &Path) -> Result<Vec<LocalFile>, Box<dyn std::error::Error>> {
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()).into());
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).follow_links(true).sort_by_file_name() {
        let entry = entry?;
        let rel = entry.path().strip_prefix(dir)?;
        if rel.starts_with(LOCAL_DIR) {
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let parts = rel
            .components()
            .map(|c| {
                c.as_os_str()
                    .to_str()
                    .ok_or(format!("non UTF-8 file name: {}", rel.display()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let path = parts.join("/");
        if path == ARTIFACTS_FILE {
            return Err(
                format!("{ARTIFACTS_FILE} in the root of the directory is reserved").into(),
            );
        }
        let meta = entry.metadata()?;
        files.push(LocalFile {
            abs: entry.path().to_path_buf(),
            artifact: Artifact {
                path,
                size: meta.len(),
                modified_at: rfc3339(meta.modified()?.into()),
                blake3: storage::utils::hash_file(entry.path())?,
            },
        });
    }
    if files.is_empty() {
        return Err(format!("no files in {}", dir.display()).into());
    }
    Ok(files)
}

fn read_versions(storage: &dyn Storage, key: &str) -> Result<Versions, Box<dyn std::error::Error>> {
    if !storage.exists(key)? {
        return Err(format!("model not initialized: {key} (run `mr init`)").into());
    }
    Ok(versions::parse(&storage.get(key)?).map_err(|e| format!("invalid {key}: {e}"))?)
}

fn push(
    storage: &dyn Storage,
    model: &str,
    version: &str,
    files: Vec<LocalFile>,
    opts: Options,
    now: DateTime<Utc>,
    out: &mut dyn Write,
) -> CmdResult {
    let Options {
        message,
        dry,
        force,
        yes,
        ask,
        jobs,
    } = opts;
    let progress_on = std::io::stderr().is_terminal();
    let versions_key = versions_key(model)?;

    let existing = read_versions(storage, &versions_key)?;
    let auto = version == AUTO_VERSION;
    let version = if auto {
        existing.next_version()?
    } else {
        version.to_string()
    };
    let version = version.as_str();
    let prefix = format!("{model}/{version}");
    if existing.is_alias(version) {
        return Err(format!("'{version}' is an alias and cannot be used as a version name").into());
    }
    if existing.find(version).is_some() {
        return Err(format!("version '{version}' already exists").into());
    }
    if !auto {
        let warnings = existing.version_warnings(version);
        for w in &warnings {
            writeln!(out, "{}{w}", super::warning_prefix())?;
        }
        if !warnings.is_empty() && !dry && !yes {
            out.flush()?;
            if !ask(&format!("Push {version} anyway? [y/N] ")) {
                return Err("aborted".into());
            }
        }
    }
    let leftovers = storage.list(&format!("{prefix}/"))?.len();
    if leftovers > 0 && !force {
        return Err(format!(
            "{prefix}/ already contains {leftovers} files (interrupted push?); use --force to delete them and retry"
        )
        .into());
    }

    let total_size: u64 = files.iter().map(|f| f.artifact.size).sum();
    let artifacts = Artifacts {
        version: version.to_string(),
        message: message.clone(),
        files: files.iter().map(|f| f.artifact.clone()).collect(),
    };
    let entry = Version {
        version: version.to_string(),
        tool_version: TOOL_VERSION.to_string(),
        path: prefix.clone(),
        created_at: rfc3339(now),
        file_count: files.len(),
        total_size,
        message,
    };

    if dry {
        if leftovers > 0 {
            writeln!(
                out,
                "[dry] would delete {leftovers} leftover files in {prefix}/"
            )?;
        }
        for f in &files {
            writeln!(
                out,
                "[dry] would upload {prefix}/{} ({} bytes)",
                f.artifact.path, f.artifact.size
            )?;
        }
        writeln!(
            out,
            "[dry] would write {prefix}/{ARTIFACTS_FILE} and add version '{version}' to {versions_key}"
        )?;
        writeln!(
            out,
            "[dry] {} files, {total_size} bytes, nothing was changed",
            entry.file_count
        )?;
        return Ok(());
    }

    if leftovers > 0 {
        let n = storage.delete_prefix(&format!("{prefix}/"))?;
        writeln!(out, "deleted {n} leftover files in {prefix}/")?;
    }

    let progress = if progress_on {
        Progress::new("uploading", files.len() as u64, total_size)
    } else {
        Progress::hidden()
    };
    let upload = parallel::run(
        &files,
        jobs,
        |f| {
            let fp = progress.file(&f.artifact.path, f.artifact.size);
            let res = storage
                .put_file(&format!("{prefix}/{}", f.artifact.path), &f.abs, &|n| {
                    fp.inc(n)
                })
                .map_err(|e| format!("{}: {e}", f.artifact.path));
            match res {
                Ok(()) => fp.done(),
                Err(_) => fp.abandon(),
            }
            res
        },
        |f, _| {
            if progress.visible() {
                return Ok(());
            }
            writeln!(
                out,
                "{}{prefix}/{} ({} bytes)",
                status("Uploaded"),
                f.artifact.path,
                f.artifact.size
            )
        },
    )
    .map_err(Into::into)
    .and_then(|_| -> CmdResult {
        storage.put(
            &format!("{prefix}/{ARTIFACTS_FILE}"),
            &serde_json::to_vec_pretty(&artifacts)?,
        )
    });
    if let Err(e) = upload {
        // nothing is registered yet, so everything under the prefix belongs to this push
        return Err(match storage.delete_prefix(&format!("{prefix}/")) {
            Ok(_) => format!("{e} (uploaded files were removed, you can retry)"),
            Err(c) => format!("{e} (cleanup failed: {c}; retry with --force)"),
        }
        .into());
    }

    // Updating versions.json is the commit point: re-read it to narrow the window
    // for a concurrent push of the same version.
    let mut versions = read_versions(storage, &versions_key)?;
    if versions.find(version).is_some() {
        return Err(format!("version '{version}' was added concurrently").into());
    }
    let (count, size) = (entry.file_count, entry.total_size);
    versions.add(entry);
    storage.put(&versions_key, &serde_json::to_vec_pretty(&versions)?)?;
    progress.finish();
    write!(
        out,
        "{}{version}: {count} files, {size} bytes to {prefix}/",
        status("Pushed")
    )?;
    if progress.visible() {
        write!(
            out,
            " in {:.1?} ({})",
            progress.elapsed(),
            speed(size, progress.elapsed())
        )?;
    }
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mem::MemStorage;
    use std::fs;

    fn now() -> DateTime<Utc> {
        "2026-11-05T10:00:00Z".parse().unwrap()
    }

    fn sample_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.bin"), b"hello").unwrap();
        fs::create_dir_all(d.path().join("sub/deep")).unwrap();
        fs::create_dir_all(d.path().join(".mr")).unwrap();
        fs::write(d.path().join(".mr/artifacts.json"), b"{}").unwrap();
        fs::write(d.path().join("sub/deep/b.txt"), b"hi").unwrap();
        d
    }

    fn init_mem() -> MemStorage {
        let mem = MemStorage::default();
        let empty = Versions::new("2026-11-05T09:00:00Z".into());
        mem.put("m/versions.json", &serde_json::to_vec(&empty).unwrap())
            .unwrap();
        mem
    }

    fn do_push(
        mem: &dyn Storage,
        version: &str,
        dir: &Path,
        dry: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        do_push_opts(
            mem,
            version,
            dir,
            Options {
                message: String::new(),
                dry,
                force: false,
                yes: true,
                ask: |_| true,
                jobs: 4,
            },
        )
    }

    fn do_push_opts(
        storage: &dyn Storage,
        version: &str,
        dir: &Path,
        opts: Options,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut out = Vec::new();
        push(storage, "m", version, scan(dir)?, opts, now(), &mut out)?;
        Ok(String::from_utf8(out)?)
    }

    /// Fails uploads of files whose key ends with `fail_suffix`.
    struct Flaky {
        inner: MemStorage,
        fail_suffix: &'static str,
    }

    impl Storage for Flaky {
        fn put(&self, k: &str, d: &[u8]) -> storage::StorageResult<()> {
            self.inner.put(k, d)
        }
        fn put_file(&self, k: &str, p: &Path, b: storage::OnBytes) -> storage::StorageResult<()> {
            if k.ends_with(self.fail_suffix) {
                return Err("network down".into());
            }
            self.inner.put_file(k, p, b)
        }
        fn get(&self, k: &str) -> storage::StorageResult<Vec<u8>> {
            self.inner.get(k)
        }
        fn get_file(&self, k: &str, p: &Path, b: storage::OnBytes) -> storage::StorageResult<()> {
            self.inner.get_file(k, p, b)
        }
        fn exists(&self, k: &str) -> storage::StorageResult<bool> {
            self.inner.exists(k)
        }
        fn list(&self, p: &str) -> storage::StorageResult<Vec<String>> {
            self.inner.list(p)
        }
        fn list_dirs(&self, p: &str) -> storage::StorageResult<Vec<String>> {
            self.inner.list_dirs(p)
        }
        fn delete_prefix(&self, p: &str) -> storage::StorageResult<usize> {
            self.inner.delete_prefix(p)
        }
    }

    #[test]
    fn uploads_files_artifacts_and_version_entry() {
        let (mem, dir) = (init_mem(), sample_dir());
        do_push(&mem, "v1", dir.path(), false).unwrap();

        assert_eq!(mem.get("m/v1/a.bin").unwrap(), b"hello");
        assert_eq!(mem.get("m/v1/sub/deep/b.txt").unwrap(), b"hi");

        let artifacts: Artifacts =
            serde_json::from_slice(&mem.get("m/v1/artifacts.json").unwrap()).unwrap();
        assert_eq!(artifacts.version, "v1");
        let artifacts = artifacts.files;
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].path, "a.bin");
        assert_eq!(artifacts[0].size, 5);
        assert_eq!(
            artifacts[0].blake3,
            blake3::hash(b"hello").to_hex().to_string()
        );
        assert_eq!(artifacts[1].path, "sub/deep/b.txt");

        let versions = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        assert_eq!(versions.latest_model_version, "v1");
        assert_eq!(
            versions.versions,
            vec![Version {
                version: "v1".into(),
                tool_version: TOOL_VERSION.into(),
                path: "m/v1".into(),
                created_at: "2026-11-05T10:00:00Z".into(),
                file_count: 2,
                total_size: 7,
                message: String::new(),
            }]
        );
    }

    fn auto_push(mem: &MemStorage, dir: &Path) -> String {
        let opts = Options {
            message: String::new(),
            dry: false,
            force: false,
            yes: true,
            ask: |_| true,
            jobs: 1,
        };
        let mut out = Vec::new();
        push(mem, "m", "auto", scan(dir).unwrap(), opts, now(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn asks_after_warning() {
        let (mem, dir) = (init_mem(), sample_dir());
        let go = |version: &str, yes: bool, ask: fn(&str) -> bool| {
            let opts = Options {
                message: String::new(),
                dry: false,
                force: false,
                yes,
                ask,
                jobs: 1,
            };
            let mut out = Vec::new();
            push(
                &mem,
                "m",
                version,
                scan(dir.path()).unwrap(),
                opts,
                now(),
                &mut out,
            )
        };
        go("v1.0.0", false, |_| panic!("no warning, no question")).unwrap();
        let err = go("v1.0.5", false, |q| {
            assert!(q.contains("Push v1.0.5 anyway? [y/N]"));
            false
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "aborted");
        let v = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        assert!(v.find("v1.0.5").is_none());
        assert!(mem.list("m/v1.0.5/").unwrap().is_empty());
        go("v1.0.5", false, |_| true).unwrap();
        go("v1.0.9", true, |_| panic!("--yes skips the question")).unwrap();
    }

    #[test]
    fn warns_on_unusual_versions() {
        let (mem, dir) = (init_mem(), sample_dir());
        let go = |version: &str| {
            let opts = Options {
                message: String::new(),
                dry: false,
                force: false,
                yes: true,
                ask: |_| true,
                jobs: 1,
            };
            let mut out = Vec::new();
            push(
                &mem,
                "m",
                version,
                scan(dir.path()).unwrap(),
                opts,
                now(),
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(!go("v1.0.0").contains("warning"));
        let out = go("v1.0.5");
        assert!(
            out.contains("warning: patch version is expected to be 1"),
            "{out}"
        );
        assert!(out.contains("Pushed v1.0.5"), "{out}");
        assert!(go("v1.0.6").lines().all(|l| !l.contains("warning")));
    }

    #[test]
    fn auto_version_starts_at_first_and_increases() {
        let (mem, dir) = (init_mem(), sample_dir());
        assert!(auto_push(&mem, dir.path()).contains("Pushed v0.0.1:"));
        assert!(auto_push(&mem, dir.path()).contains("Pushed v0.0.2:"));
        let v = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        assert_eq!(v.latest_model_version, "v0.0.2");
        // an explicit version continues the sequence
        do_push(&mem, "v1.0.0", dir.path(), false).unwrap();
        assert!(auto_push(&mem, dir.path()).contains("Pushed v1.0.1:"));
    }

    #[test]
    fn auto_is_reserved() {
        assert!(validate_version("auto").is_err());
        assert!(
            crate::commands::parse_for_test(Push::new().definition(), &["push", "m:v1", "-b", "x"])
                .is_err()
        );
    }

    #[test]
    fn stores_message() {
        let (mem, dir) = (init_mem(), sample_dir());
        let opts = Options {
            message: "better loss function".into(),
            dry: false,
            force: false,
            yes: true,
            ask: |_| true,
            jobs: 1,
        };
        do_push_opts(&mem, "v1", dir.path(), opts).unwrap();
        let artifacts =
            crate::models::artifacts::parse(&mem.get("m/v1/artifacts.json").unwrap()).unwrap();
        assert_eq!(artifacts.message, "better loss function");
        let versions = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        assert_eq!(versions.versions[0].message, "better loss function");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (mem, dir) = (init_mem(), sample_dir());
        let out = do_push(&mem, "v1", dir.path(), true).unwrap();
        assert!(out.contains("[dry] would upload m/v1/a.bin"));
        assert_eq!(mem.len(), 1);
        let v = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        assert!(v.versions.is_empty());
        assert_eq!(v.latest_model_version, "");
    }

    #[test]
    fn rejects_existing_version() {
        let (mem, dir) = (init_mem(), sample_dir());
        do_push(&mem, "v1", dir.path(), false).unwrap();
        assert!(
            do_push(&mem, "v1", dir.path(), false)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(do_push(&mem, "v1", dir.path(), true).is_err());
    }

    #[test]
    fn leftover_files_need_force() {
        let (mem, dir) = (init_mem(), sample_dir());
        mem.put("m/v2/old.bin", b"x").unwrap();
        let err = do_push(&mem, "v2", dir.path(), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already contains 1 files") && err.contains("--force"),
            "{err}"
        );

        let force = Options {
            message: String::new(),
            dry: false,
            force: true,
            yes: true,
            ask: |_| true,
            jobs: 2,
        };
        let out = do_push_opts(&mem, "v2", dir.path(), force).unwrap();
        assert!(out.contains("deleted 1 leftover files"));
        assert!(!mem.exists("m/v2/old.bin").unwrap());
        assert!(mem.exists("m/v2/a.bin").unwrap());
        assert!(
            versions::parse(&mem.get("m/versions.json").unwrap())
                .unwrap()
                .find("v2")
                .is_some()
        );
    }

    #[test]
    fn dry_force_deletes_nothing() {
        let (mem, dir) = (init_mem(), sample_dir());
        mem.put("m/v2/old.bin", b"x").unwrap();
        let opts = Options {
            message: String::new(),
            dry: true,
            force: true,
            yes: true,
            ask: |_| true,
            jobs: 1,
        };
        let out = do_push_opts(&mem, "v2", dir.path(), opts).unwrap();
        assert!(out.contains("[dry] would delete 1 leftover files"));
        assert!(mem.exists("m/v2/old.bin").unwrap());
    }

    #[test]
    fn failed_upload_is_cleaned_up_and_retryable() {
        let dir = sample_dir();
        let flaky = Flaky {
            inner: init_mem(),
            fail_suffix: "b.txt",
        };
        let err = do_push(&flaky, "v1", dir.path(), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("network down") && err.contains("removed"),
            "{err}"
        );
        // only versions.json is left, and it has no entry
        assert_eq!(flaky.inner.len(), 1);
        assert!(
            versions::parse(&flaky.inner.get("m/versions.json").unwrap())
                .unwrap()
                .versions
                .is_empty()
        );

        let ok = Flaky {
            inner: flaky.inner.clone(),
            fail_suffix: "never",
        };
        do_push(&ok, "v1", dir.path(), false).unwrap();
        assert!(ok.exists("m/v1/artifacts.json").unwrap());
    }

    #[test]
    fn requires_init() {
        let dir = sample_dir();
        let err = do_push(&MemStorage::default(), "v1", dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn rejects_bad_dirs_and_versions() {
        let empty = tempfile::tempdir().unwrap();
        assert!(scan(empty.path()).is_err());
        assert!(scan(Path::new("/no/such/dir")).is_err());
        let d = sample_dir();
        fs::write(d.path().join("artifacts.json"), b"{}").unwrap();
        assert!(scan(d.path()).is_err());
        assert!(validate_version("a/b").is_err());
        assert!(validate_version("").is_err());
    }
}
